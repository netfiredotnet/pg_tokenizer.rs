use std::{
    collections::HashSet,
    sync::{Arc, LazyLock},
};

use dashmap::{DashMap, Entry};
use serde::{Deserialize, Serialize};

use crate::utils::spi_get_one;

use super::TokenFilter;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StopwordsTokenFilter {
    stopwords: HashSet<String>,
}
pub type StopwordsTokenFilterPtr = Arc<StopwordsTokenFilter>;

impl StopwordsTokenFilter {
    // config is a string with multiple lines, each line represents a stopword
    pub fn build(config: &str) -> Self {
        let mut stopwords = HashSet::new();

        for line in config.lines() {
            stopwords.insert(line.to_string());
        }

        StopwordsTokenFilter { stopwords }
    }
}

impl TokenFilter for StopwordsTokenFilter {
    fn apply(&self, token: String) -> Vec<String> {
        if self.stopwords.contains(token.as_str()) {
            vec![]
        } else {
            vec![token]
        }
    }
}

pgrx::extension_sql!(
    r#"
CREATE TABLE tokenizer_catalog.stopwords (
    name TEXT NOT NULL UNIQUE PRIMARY KEY,
    config TEXT NOT NULL,
    owner NAME NOT NULL DEFAULT CURRENT_USER
);
"#,
    name = "stopwords_table"
);

type StopwordsObjectPool = DashMap<String, StopwordsTokenFilterPtr>;
static STOPWORDS_OBJECT_POOL: LazyLock<StopwordsObjectPool> =
    LazyLock::new(StopwordsObjectPool::default);

pub fn get_stopwords_token_filter(name: &str) -> StopwordsTokenFilterPtr {
    if let Some(model) = STOPWORDS_OBJECT_POOL.get(name) {
        return model.clone();
    }

    match STOPWORDS_OBJECT_POOL.entry(name.to_string()) {
        Entry::Occupied(entry) => entry.get().clone(),
        Entry::Vacant(entry) => {
            if let Some(object) = get_stopwords_token_filter_from_database(name) {
                entry.insert(object.clone());
                return object;
            }

            panic!("Stopwords not found: {}", name);
        }
    }
}

fn get_stopwords_token_filter_from_database(name: &str) -> Option<StopwordsTokenFilterPtr> {
    let config: &str = spi_get_one(
        "SELECT config FROM tokenizer_catalog.stopwords WHERE name = $1",
        &[name.into()],
    )?;

    let stopwords = StopwordsTokenFilter::build(config);
    Some(Arc::new(stopwords))
}

#[pgrx::pg_extern(
    name = "__pg_tokenizer_create_stopwords",
    volatile,
    parallel_safe,
    security_definer,
)]
#[pgrx::search_path(tokenizer_catalog, pg_catalog)]
fn create_stopwords_internal(name: &str, config: &str, owner: &str) {
    let stopwords = StopwordsTokenFilter::build(config);

    pgrx::Spi::connect_mut(|client| {
        let tuptable = client
            .update(
                r#"
                INSERT INTO tokenizer_catalog.stopwords (name, config, owner) VALUES ($1, $2, $3)
                ON CONFLICT (name) DO NOTHING RETURNING 1
                "#,
                Some(1),
                &[name.into(), config.into(), owner.into()],
            )
            .unwrap();

        if tuptable.is_empty() {
            panic!("Stopwords already exists: {}", name);
        }

        if STOPWORDS_OBJECT_POOL
            .insert(name.to_string(), Arc::new(stopwords))
            .is_some()
        {
            panic!("Stopwords already exists: {}", name);
        }
    });
}

#[pgrx::pg_extern(
    name = "__pg_tokenizer_drop_stopwords",
    volatile,
    parallel_safe,
    security_definer,
)]
#[pgrx::search_path(tokenizer_catalog, pg_catalog)]
fn drop_stopwords_internal(name: &str, owner: &str) {
    pgrx::Spi::connect_mut(|client| {
        let tuptable = client
            .update(
                "DELETE FROM tokenizer_catalog.stopwords WHERE name = $1 AND owner = $2 RETURNING 1",
                Some(1),
                &[name.into(), owner.into()],
            )
            .unwrap();

        if tuptable.is_empty() {
            panic!("Stopwords not found or not owned by current user: {}", name);
        }
    });

    STOPWORDS_OBJECT_POOL.remove(name);
}

macro_rules! STOPWORDS_DIR {
    () => {
        concat!(env!("CARGO_MANIFEST_DIR"), "/assets/stopwords")
    };
}

static LUCENE_ENGLISH_STOPWORDS: &str = include_str!(concat!(STOPWORDS_DIR!(), "/lucene_english"));
static NLTK_ENGLISH_STOPWORDS: &str = include_str!(concat!(STOPWORDS_DIR!(), "/nltk_english"));
static ISO_ENGLISH_STOPWORDS: &str = include_str!(concat!(STOPWORDS_DIR!(), "/iso_english"));

fn create_stopwords_when_init(name: &str, config: &str) {
    pgrx::Spi::connect_mut(|client| {
        client
            .update(
                r#"
                INSERT INTO tokenizer_catalog.stopwords (name, config) VALUES ($1, $2)
                ON CONFLICT (name) DO NOTHING
                "#,
                Some(1),
                &[name.into(), config.into()],
            )
            .unwrap();
    });
}

#[pgrx::pg_extern]
pub fn _pg_tokenizer_stopwords_init() {
    create_stopwords_when_init("lucene_english", LUCENE_ENGLISH_STOPWORDS);
    create_stopwords_when_init("nltk_english", NLTK_ENGLISH_STOPWORDS);
    create_stopwords_when_init("iso_english", ISO_ENGLISH_STOPWORDS);
}

pgrx::extension_sql!(
    r#"
    SELECT tokenizer_catalog._pg_tokenizer_stopwords_init();
    "#,
    name = "stopwords_init",
    requires = ["stopwords_table", _pg_tokenizer_stopwords_init]
);

pgrx::extension_sql!(
    r#"
CREATE FUNCTION tokenizer_catalog.create_stopwords(name TEXT, config TEXT)
RETURNS VOID
LANGUAGE sql VOLATILE PARALLEL SAFE SECURITY DEFINER
SET search_path = tokenizer_catalog, pg_catalog
AS $$ SELECT tokenizer_catalog.__pg_tokenizer_create_stopwords($1, $2, CASE WHEN pg_catalog.current_setting('role') = 'none' THEN session_user::text ELSE pg_catalog.current_setting('role') END); $$;
"#,
    name = "create_stopwords_wrapper_sql",
    requires = [create_stopwords_internal]
);

pgrx::extension_sql!(
    r#"
CREATE FUNCTION tokenizer_catalog.drop_stopwords(name TEXT)
RETURNS VOID
LANGUAGE sql VOLATILE PARALLEL SAFE SECURITY DEFINER
SET search_path = tokenizer_catalog, pg_catalog
AS $$ SELECT tokenizer_catalog.__pg_tokenizer_drop_stopwords($1, CASE WHEN pg_catalog.current_setting('role') = 'none' THEN session_user::text ELSE pg_catalog.current_setting('role') END); $$;
"#,
    name = "drop_stopwords_wrapper_sql",
    requires = [drop_stopwords_internal]
);
