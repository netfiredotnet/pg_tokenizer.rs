use std::sync::Arc;

use lindera::tokenizer::{Tokenizer, TokenizerConfig};
use serde::{Deserialize, Serialize};

use super::{validate_new_model_name, ModelConfig, TokenizerModel, MODEL_OBJECT_POOL};

#[derive(Debug, Serialize, Deserialize)]

pub struct LinderaConfig {
    #[serde(flatten)]
    inner: TokenizerConfig,
}

pub struct LinderaModel {
    tokenizer: Tokenizer,
}

impl LinderaModel {
    pub fn new(config: &LinderaConfig) -> Self {
        let tokenizer = Tokenizer::from_config(&config.inner).unwrap();
        Self { tokenizer }
    }
}

impl TokenizerModel for LinderaModel {
    fn apply(&self, token: String) -> Vec<u32> {
        self.tokenizer
            .tokenize(&token)
            .unwrap()
            .into_iter()
            .map(|t| t.word_id.id)
            .filter(|id| *id != u32::MAX)
            .collect()
    }
}

#[pgrx::pg_extern(
    name = "__pg_tokenizer_create_lindera_model",
    volatile,
    parallel_safe,
    security_definer,
)]
#[pgrx::search_path(tokenizer_catalog, pg_catalog)]
fn create_lindera_model_internal(name: &str, config: &str, owner: &str) {
    validate_new_model_name(name).unwrap();
    let config: LinderaConfig = toml::from_str(config).unwrap();

    let insert_model = r#"
        INSERT INTO tokenizer_catalog.model (name, config, owner) VALUES ($1, $2, $3)
        ON CONFLICT (name) DO NOTHING RETURNING 1
    "#;

    let lindera_model = LinderaModel::new(&config);
    let config_str = serde_json::to_string(&ModelConfig::Lindera(config)).unwrap();

    pgrx::Spi::connect_mut(|client| {
        let tuptable = client
            .update(
                insert_model,
                Some(1),
                &[name.into(), config_str.into(), owner.into()],
            )
            .unwrap();

        if tuptable.is_empty() {
            panic!("Model already exists: {}", name);
        }

        MODEL_OBJECT_POOL.insert(name.to_string(), Arc::new(lindera_model));
    });
}

#[pgrx::pg_extern(
    name = "__pg_tokenizer_drop_lindera_model",
    volatile,
    parallel_safe,
    security_definer,
)]
#[pgrx::search_path(tokenizer_catalog, pg_catalog)]
fn drop_lindera_model_internal(name: &str, owner: &str) {
    validate_new_model_name(name).unwrap();

    let delete_model = r#"
        DELETE FROM tokenizer_catalog.model WHERE name = $1 AND owner = $2 RETURNING 1
    "#;

    pgrx::Spi::connect_mut(|client| {
        let tuptable = client
            .update(delete_model, Some(1), &[name.into(), owner.into()])
            .unwrap();

        if tuptable.is_empty() {
            panic!("Model not found or not owned by current user: {}", name);
        }
    });

    MODEL_OBJECT_POOL.remove(name);
}

pgrx::extension_sql!(
    r#"
CREATE FUNCTION tokenizer_catalog.create_lindera_model(name TEXT, config TEXT)
RETURNS VOID
LANGUAGE sql VOLATILE PARALLEL SAFE SECURITY DEFINER
SET search_path = tokenizer_catalog, pg_catalog
AS $$ SELECT tokenizer_catalog.__pg_tokenizer_create_lindera_model($1, $2, CASE WHEN pg_catalog.current_setting('role') = 'none' THEN session_user::text ELSE pg_catalog.current_setting('role') END); $$;
"#,
    name = "create_lindera_model_wrapper_sql",
    requires = [create_lindera_model_internal]
);

pgrx::extension_sql!(
    r#"
CREATE FUNCTION tokenizer_catalog.drop_lindera_model(name TEXT)
RETURNS VOID
LANGUAGE sql VOLATILE PARALLEL SAFE SECURITY DEFINER
SET search_path = tokenizer_catalog, pg_catalog
AS $$ SELECT tokenizer_catalog.__pg_tokenizer_drop_lindera_model($1, CASE WHEN pg_catalog.current_setting('role') = 'none' THEN session_user::text ELSE pg_catalog.current_setting('role') END); $$;
"#,
    name = "drop_lindera_model_wrapper_sql",
    requires = [drop_lindera_model_internal]
);
