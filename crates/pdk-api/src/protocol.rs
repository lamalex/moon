use warpgate_api::api_struct;

api_struct!(
    /// Exact identity of a plugin protocol contract.
    pub struct ProtocolIdentity {
        /// Generated fingerprint of the serialized wire contract.
        pub fingerprint: String,
    }
);

/// Defines a plugin protocol with generated structural compatibility data.
pub trait PluginProtocol {
    const FINGERPRINT: &'static str;

    fn identity() -> ProtocolIdentity {
        ProtocolIdentity {
            fingerprint: Self::FINGERPRINT.into(),
        }
    }

    fn is_compatible(identity: &ProtocolIdentity) -> bool {
        identity == &Self::identity()
    }
}
