use std::sync::Arc;

use tokenizers::Tokenizer;

use super::{validate_new_model_name, ModelConfig, TokenizerModel, MODEL_OBJECT_POOL};

#[derive(Debug)]
pub struct HuggingFaceModel {
    tokenizer: Tokenizer,
}

impl HuggingFaceModel {
    pub fn new(_name: &str, config: &HuggingFaceConfig) -> Self {
        let tokenizer = Tokenizer::from_bytes(config.as_bytes()).expect("Failed to load tokenizer");
        HuggingFaceModel { tokenizer }
    }
}

impl TokenizerModel for HuggingFaceModel {
    fn apply(&self, text: String) -> Vec<u32> {
        self.tokenizer.apply(text)
    }

    fn apply_batch(&self, tokens: Vec<String>) -> Vec<u32> {
        self.tokenizer.apply_batch(tokens)
    }
}

pub type HuggingFaceConfig = String;

#[pgrx::pg_extern(
    name = "__pg_tokenizer_create_huggingface_model",
    volatile,
    parallel_safe,
    security_definer,
)]
#[pgrx::search_path(tokenizer_catalog, pg_catalog)]
fn create_huggingface_model_internal(name: &str, config: &str, owner: &str) {
    validate_new_model_name(name).unwrap();

    let insert_model = r#"
        INSERT INTO tokenizer_catalog.model (name, config, owner) VALUES ($1, $2, $3)
        ON CONFLICT (name) DO NOTHING RETURNING 1
    "#;

    let config = config.to_string();
    let model = HuggingFaceModel::new(name, &config);
    let config_str = serde_json::to_string(&ModelConfig::HuggingFace(config)).unwrap();

    pgrx::Spi::connect_mut(|client| {
        let tuptable = client
            .update(insert_model, Some(1), &[name.into(), config_str.into(), owner.into()])
            .unwrap();

        if tuptable.is_empty() {
            panic!("Model already exists: {}", name);
        }

        if MODEL_OBJECT_POOL
            .insert(name.to_string(), Arc::new(model))
            .is_some()
        {
            panic!("Model already exists: {}", name);
        }
    });
}

#[pgrx::pg_extern(
    name = "__pg_tokenizer_drop_huggingface_model",
    volatile,
    parallel_safe,
    security_definer,
)]
#[pgrx::search_path(tokenizer_catalog, pg_catalog)]
fn drop_huggingface_model_internal(name: &str, owner: &str) {
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
CREATE FUNCTION tokenizer_catalog.create_huggingface_model(name TEXT, config TEXT)
RETURNS VOID
LANGUAGE sql VOLATILE PARALLEL SAFE SECURITY DEFINER
SET search_path = tokenizer_catalog, pg_catalog
AS $$ SELECT tokenizer_catalog.__pg_tokenizer_create_huggingface_model($1, $2, CASE WHEN pg_catalog.current_setting('role') = 'none' THEN session_user::text ELSE pg_catalog.current_setting('role') END); $$;
"#,
    name = "create_huggingface_model_wrapper_sql",
    requires = [create_huggingface_model_internal]
);

pgrx::extension_sql!(
    r#"
CREATE FUNCTION tokenizer_catalog.drop_huggingface_model(name TEXT)
RETURNS VOID
LANGUAGE sql VOLATILE PARALLEL SAFE SECURITY DEFINER
SET search_path = tokenizer_catalog, pg_catalog
AS $$ SELECT tokenizer_catalog.__pg_tokenizer_drop_huggingface_model($1, CASE WHEN pg_catalog.current_setting('role') = 'none' THEN session_user::text ELSE pg_catalog.current_setting('role') END); $$;
"#,
    name = "drop_huggingface_model_wrapper_sql",
    requires = [drop_huggingface_model_internal]
);
