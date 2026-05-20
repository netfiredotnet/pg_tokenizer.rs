use std::sync::{Arc, LazyLock};

use dashmap::{DashMap, Entry};
use serde::{Deserialize, Serialize};
use validator::{Validate, ValidationError};

use crate::{
    character_filter::CharacterFilterConfig,
    model::{get_model, TokenizerModelPtr},
    pre_tokenizer::PreTokenizerConfig,
    text_analyzer::{get_text_analyzer, TextAnalyzer, TextAnalyzerConfig, TextAnalyzerPtr},
    token_filter::TokenFilterConfig,
    utils::spi_get_one,
};

#[derive(Clone, Serialize, Deserialize, Validate)]
#[validate(schema(function = "TokenizerConfig::validate_text_analyzer"))]
#[serde(deny_unknown_fields)]
struct TokenizerConfig {
    #[serde(default)]
    text_analyzer: Option<String>,
    #[serde(default)]
    character_filters: Vec<CharacterFilterConfig>,
    #[serde(default)]
    pre_tokenizer: Option<PreTokenizerConfig>,
    #[serde(default)]
    token_filters: Vec<TokenFilterConfig>,
    model: String,
}

impl TokenizerConfig {
    fn validate_text_analyzer(&self) -> Result<(), ValidationError> {
        let external_defined = self.text_analyzer.is_some();
        let inline_defined = !self.character_filters.is_empty()
            || self.pre_tokenizer.is_some()
            || !self.token_filters.is_empty();

        if external_defined && inline_defined {
            return Err(ValidationError::new(
                "cannot define both text_analyzer and inlined text_analyzer options",
            ));
        }

        Ok(())
    }
}

pub struct Tokenizer {
    pub text_analyzer: TextAnalyzerPtr,
    pub model: TokenizerModelPtr,
}
pub type TokenizerPtr = Arc<Tokenizer>;

impl Tokenizer {
    fn build(config: TokenizerConfig) -> Tokenizer {
        let text_analyzer = match config.text_analyzer {
            Some(name) => get_text_analyzer(&name),
            None => Arc::new(TextAnalyzer::build(TextAnalyzerConfig {
                character_filters: config.character_filters,
                pre_tokenizer: config.pre_tokenizer,
                token_filters: config.token_filters,
            })),
        };

        let model = get_model(&config.model);

        Tokenizer {
            text_analyzer,
            model,
        }
    }

    pub fn tokenize(&self, text: &str) -> Vec<u32> {
        let tokens = self.text_analyzer.apply(text);
        self.model.apply_batch(tokens)
    }
}

type TokenizerObjectPool = DashMap<String, TokenizerPtr>;
static TOKENIZER_OBJECT_POOL: LazyLock<TokenizerObjectPool> =
    LazyLock::new(TokenizerObjectPool::default);

pgrx::extension_sql!(
    r#"
CREATE TABLE tokenizer_catalog.tokenizer (
    name TEXT NOT NULL UNIQUE PRIMARY KEY,
    config TEXT NOT NULL,
    owner NAME NOT NULL DEFAULT CURRENT_USER
);
"#,
    name = "tokenizer_table"
);

pub fn get_tokenizer(name: &str) -> TokenizerPtr {
    if let Some(model) = TOKENIZER_OBJECT_POOL.get(name) {
        return model.clone();
    }

    match TOKENIZER_OBJECT_POOL.entry(name.to_string()) {
        Entry::Occupied(entry) => entry.get().clone(),
        Entry::Vacant(entry) => {
            if let Some(object) = get_tokenizer_from_database(name) {
                entry.insert(object.clone());
                return object;
            }

            panic!("Tokenizer not found: {}", name);
        }
    }
}

fn get_tokenizer_from_database(name: &str) -> Option<TokenizerPtr> {
    let config_bytes: &str = spi_get_one(
        "SELECT config FROM tokenizer_catalog.tokenizer WHERE name = $1",
        &[name.into()],
    )?;

    let config: TokenizerConfig = serde_json::from_str(config_bytes).unwrap();
    Some(Arc::new(Tokenizer::build(config)))
}

#[pgrx::pg_extern(
    name = "__pg_tokenizer_create_tokenizer",
    volatile,
    parallel_safe,
    security_definer,
)]
#[pgrx::search_path(tokenizer_catalog, pg_catalog)]
fn create_tokenizer_internal(name: &str, config: &str, owner: &str) {
    let config: TokenizerConfig = toml::from_str(config).unwrap();
    config.validate().unwrap();

    let config_str = serde_json::to_string(&config).unwrap();
    let tokenizer = Tokenizer::build(config);

    pgrx::Spi::connect_mut(|client| {
        let tuptable = client
            .update(
                r#"
                INSERT INTO tokenizer_catalog.tokenizer (name, config, owner) VALUES ($1, $2, $3)
                ON CONFLICT (name) DO NOTHING RETURNING 1
                "#,
                Some(1),
                &[name.into(), config_str.into(), owner.into()],
            )
            .unwrap();

        if tuptable.is_empty() {
            panic!("Tokenizer already exists: {}", name);
        }

        TOKENIZER_OBJECT_POOL.insert(name.to_string(), Arc::new(tokenizer));
    });
}

#[pgrx::pg_extern(
    name = "__pg_tokenizer_drop_tokenizer",
    volatile,
    parallel_safe,
    security_definer,
)]
#[pgrx::search_path(tokenizer_catalog, pg_catalog)]
fn drop_tokenizer_internal(name: &str, owner: &str) {
    pgrx::Spi::connect_mut(|client| {
        let tuptable = client
            .update(
                "DELETE FROM tokenizer_catalog.tokenizer WHERE name = $1 AND owner = $2 RETURNING 1",
                Some(1),
                &[name.into(), owner.into()],
            )
            .unwrap();

        if tuptable.is_empty() {
            panic!("Tokenizer not found or not owned by current user: {}", name);
        }
    });

    TOKENIZER_OBJECT_POOL.remove(name);
}

#[pgrx::pg_extern(
    name = "__pg_tokenizer_tokenize",
    stable,
    parallel_safe,
    security_definer,
)]
#[pgrx::search_path(tokenizer_catalog, pg_catalog)]
pub fn tokenize_internal(text: &str, tokenizer_name: &str) -> Vec<i32> {
    let tokenizer = get_tokenizer(tokenizer_name);
    tokenizer
        .tokenize(text)
        .into_iter()
        .map(|x| x.try_into().unwrap())
        .collect()
}

pgrx::extension_sql!(
    r#"
CREATE FUNCTION tokenizer_catalog.create_tokenizer(name TEXT, config TEXT)
RETURNS VOID
LANGUAGE sql VOLATILE PARALLEL SAFE SECURITY DEFINER
SET search_path = tokenizer_catalog, pg_catalog
AS $$ SELECT tokenizer_catalog.__pg_tokenizer_create_tokenizer($1, $2, CASE WHEN pg_catalog.current_setting('role') = 'none' THEN session_user::text ELSE pg_catalog.current_setting('role') END); $$;
"#,
    name = "create_tokenizer_wrapper_sql",
    requires = [create_tokenizer_internal]
);

pgrx::extension_sql!(
    r#"
CREATE FUNCTION tokenizer_catalog.drop_tokenizer(name TEXT)
RETURNS VOID
LANGUAGE sql VOLATILE PARALLEL SAFE SECURITY DEFINER
SET search_path = tokenizer_catalog, pg_catalog
AS $$ SELECT tokenizer_catalog.__pg_tokenizer_drop_tokenizer($1, CASE WHEN pg_catalog.current_setting('role') = 'none' THEN session_user::text ELSE pg_catalog.current_setting('role') END); $$;
"#,
    name = "drop_tokenizer_wrapper_sql",
    requires = [drop_tokenizer_internal]
);

pgrx::extension_sql!(
    r#"
CREATE FUNCTION tokenizer_catalog.tokenize(text TEXT, tokenizer_name TEXT)
RETURNS INTEGER[]
LANGUAGE sql STABLE PARALLEL SAFE SECURITY DEFINER
SET search_path = tokenizer_catalog, pg_catalog
AS $$ SELECT tokenizer_catalog.__pg_tokenizer_tokenize($1, $2); $$;
"#,
    name = "tokenize_wrapper_sql",
    requires = [tokenize_internal]
);
