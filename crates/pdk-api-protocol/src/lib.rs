//! Generates and verifies plugin protocol manifests.

use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

#[derive(Default)]
pub struct ProtocolManifest {
    optional: BTreeMap<String, Value>,
    required: BTreeMap<String, Value>,
}

impl ProtocolManifest {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn optional<I, O>(
        mut self,
        name: impl Into<String>,
        inputs: impl IntoIterator<Item = I>,
        outputs: impl IntoIterator<Item = O>,
    ) -> Self
    where
        I: Serialize,
        O: Serialize,
    {
        self.optional.insert(name.into(), endpoint(inputs, outputs));
        self
    }

    pub fn required<I, O>(
        mut self,
        name: impl Into<String>,
        inputs: impl IntoIterator<Item = I>,
        outputs: impl IntoIterator<Item = O>,
    ) -> Self
    where
        I: Serialize,
        O: Serialize,
    {
        self.required.insert(name.into(), endpoint(inputs, outputs));
        self
    }

    pub fn fingerprint(&self) -> String {
        fingerprint(&serde_json::to_string(&self.protocol()).unwrap())
    }

    pub fn render(&self) -> String {
        let manifest = json!({
            "wireFingerprint": self.fingerprint(),
            "protocol": self.protocol(),
        });

        format!("{}\n", serde_json::to_string_pretty(&manifest).unwrap())
    }

    pub fn update(
        &self,
        manifest_path: impl AsRef<Path>,
        fingerprint_path: impl AsRef<Path>,
        constant_name: &str,
    ) {
        fs::write(manifest_path, self.render()).unwrap();
        fs::write(
            fingerprint_path,
            render_fingerprint_constant(constant_name, &self.fingerprint()),
        )
        .unwrap();
    }

    pub fn verify(
        &self,
        manifest_path: impl AsRef<Path>,
        fingerprint_path: impl AsRef<Path>,
        constant_name: &str,
    ) {
        assert_eq!(
            fs::read_to_string(manifest_path).unwrap(),
            self.render(),
            "regenerate the plugin protocol manifest"
        );
        assert_eq!(
            fs::read_to_string(fingerprint_path).unwrap(),
            render_fingerprint_constant(constant_name, &self.fingerprint()),
            "regenerate the plugin protocol fingerprint"
        );
    }

    fn protocol(&self) -> Value {
        json!({
            "required": self.required,
            "optional": self.optional,
        })
    }
}

fn endpoint<I, O>(
    inputs: impl IntoIterator<Item = I>,
    outputs: impl IntoIterator<Item = O>,
) -> Value
where
    I: Serialize,
    O: Serialize,
{
    json!({
        "inputs": inputs.into_iter().collect::<Vec<_>>(),
        "outputs": outputs.into_iter().collect::<Vec<_>>(),
    })
}

fn fingerprint(manifest: &str) -> String {
    Sha256::digest(manifest.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn render_fingerprint_constant(name: &str, fingerprint: &str) -> String {
    format!(
        "// Generated protocol fingerprint.\n\
         pub const {name}: &str =\n    \"{fingerprint}\";\n"
    )
}
