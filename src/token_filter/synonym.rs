use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, LazyLock},
};

use dashmap::{DashMap, Entry};
use serde::{Deserialize, Serialize};

use crate::utils::spi_get_one;

use super::TokenFilter;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SynonymTokenFilter {
    synonyms: HashMap<String, String>,
}
pub type SynonymTokenFilterPtr = Arc<SynonymTokenFilter>;

impl SynonymTokenFilter {
    // config is a string with multiple lines, each line represents a group of synonyms for a single word, separated by spaces
    pub fn build(config: &str) -> Self {
        let mut synonyms = HashMap::new();
        let mut duplicate_check = HashSet::new();

        for line in config.lines() {
            let mut words = line.split_whitespace();
            if let Some(first) = words.next() {
                if !duplicate_check.insert(first.to_string()) {
                    panic!("Duplicate word defined: {}", first);
                }
                for word in words {
                    if !duplicate_check.insert(word.to_string()) {
                        panic!("Duplicate word defined: {}", word);
                    }
                    synonyms.insert(word.to_string(), first.to_string());
                }
            }
        }

        SynonymTokenFilter { synonyms }
    }
}

impl TokenFilter for SynonymTokenFilter {
    fn apply(&self, token: String) -> Vec<String> {
        if let Some(synonym) = self.synonyms.get(&token) {
            vec![synonym.clone()]
        } else {
            vec![token]
        }
    }
}

pgrx::extension_sql!(
    r#"
CREATE TABLE tokenizer_catalog.synonym (
    name TEXT NOT NULL UNIQUE PRIMARY KEY,
    config TEXT NOT NULL,
    owner NAME NOT NULL DEFAULT CURRENT_USER
);
"#,
    name = "synonym_table"
);

type SynonymObjectPool = DashMap<String, SynonymTokenFilterPtr>;
static SYNONYM_OBJECT_POOL: LazyLock<SynonymObjectPool> = LazyLock::new(SynonymObjectPool::default);

pub fn get_synonym_token_filter(name: &str) -> SynonymTokenFilterPtr {
    if let Some(model) = SYNONYM_OBJECT_POOL.get(name) {
        return model.clone();
    }

    match SYNONYM_OBJECT_POOL.entry(name.to_string()) {
        Entry::Occupied(entry) => entry.get().clone(),
        Entry::Vacant(entry) => {
            if let Some(object) = get_synonym_token_filter_from_database(name) {
                entry.insert(object.clone());
                return object;
            }

            panic!("Synonym not found: {}", name);
        }
    }
}

fn get_synonym_token_filter_from_database(name: &str) -> Option<SynonymTokenFilterPtr> {
    let config: &str = spi_get_one(
        "SELECT config FROM tokenizer_catalog.synonym WHERE name = $1",
        &[name.into()],
    )?;

    let synonym = SynonymTokenFilter::build(config);
    Some(Arc::new(synonym))
}

#[pgrx::pg_extern(
    name = "__pg_tokenizer_create_synonym",
    volatile,
    parallel_safe,
    security_definer,
)]
#[pgrx::search_path(tokenizer_catalog, pg_catalog)]
fn create_synonym_internal(name: &str, config: &str, owner: &str) {
    let synonym = SynonymTokenFilter::build(config);

    pgrx::Spi::connect_mut(|client| {
        let tuptable = client
            .update(
                r#"
                INSERT INTO tokenizer_catalog.synonym (name, config, owner) VALUES ($1, $2, $3)
                ON CONFLICT (name) DO NOTHING RETURNING 1
                "#,
                Some(1),
                &[name.into(), config.into(), owner.into()],
            )
            .unwrap();

        if tuptable.is_empty() {
            panic!("Synonym already exists: {}", name);
        }

        if SYNONYM_OBJECT_POOL
            .insert(name.to_string(), Arc::new(synonym))
            .is_some()
        {
            panic!("Synonym already exists: {}", name);
        }
    });
}

#[pgrx::pg_extern(
    name = "__pg_tokenizer_drop_synonym",
    volatile,
    parallel_safe,
    security_definer,
)]
#[pgrx::search_path(tokenizer_catalog, pg_catalog)]
fn drop_synonym_internal(name: &str, owner: &str) {
    pgrx::Spi::connect_mut(|client| {
        let tuptable = client
            .update(
                "DELETE FROM tokenizer_catalog.synonym WHERE name = $1 AND owner = $2 RETURNING 1",
                Some(1),
                &[name.into(), owner.into()],
            )
            .unwrap();

        if tuptable.is_empty() {
            panic!("Synonym not found or not owned by current user: {}", name);
        }
    });

    SYNONYM_OBJECT_POOL.remove(name);
}

pgrx::extension_sql!(
    r#"
CREATE FUNCTION tokenizer_catalog.create_synonym(name TEXT, config TEXT)
RETURNS VOID
LANGUAGE sql VOLATILE PARALLEL SAFE SECURITY DEFINER
SET search_path = tokenizer_catalog, pg_catalog
AS $$ SELECT tokenizer_catalog.__pg_tokenizer_create_synonym($1, $2, CASE WHEN pg_catalog.current_setting('role') = 'none' THEN session_user::text ELSE pg_catalog.current_setting('role') END); $$;
"#,
    name = "create_synonym_wrapper_sql",
    requires = [create_synonym_internal]
);

pgrx::extension_sql!(
    r#"
CREATE FUNCTION tokenizer_catalog.drop_synonym(name TEXT)
RETURNS VOID
LANGUAGE sql VOLATILE PARALLEL SAFE SECURITY DEFINER
SET search_path = tokenizer_catalog, pg_catalog
AS $$ SELECT tokenizer_catalog.__pg_tokenizer_drop_synonym($1, CASE WHEN pg_catalog.current_setting('role') = 'none' THEN session_user::text ELSE pg_catalog.current_setting('role') END); $$;
"#,
    name = "drop_synonym_wrapper_sql",
    requires = [drop_synonym_internal]
);
