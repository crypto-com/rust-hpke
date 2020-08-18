use crate::kex::KeyExchange;

/// Contains preshared key bytes and an identifier. This is intended to go inside an `OpModeR` or
/// `OpModeS` struct.
#[derive(Clone, Copy)]
pub struct PskBundle<'a> {
    /// The preshared key
    pub psk: &'a [u8],
    /// A bytestring that uniquely identifies this PSK
    pub psk_id: &'a [u8],
}

/// The operation mode of the HPKE session (receiver's view). This is how the sender authenticates
/// their identity to the receiver. This authentication information can include a preshared key,
/// the identity key of the sender, both, or neither. `Base` is the only mode that does not provide
/// any kind of sender identity authentication.
pub enum OpModeR<'a, Kex: KeyExchange> {
    /// No extra information included
    Base,
    /// A preshared key known to the sender and receiver
    Psk(PskBundle<'a>),
    /// The identity public key of the sender
    Auth(Kex::PublicKey),
    /// Both of the above
    AuthPsk(Kex::PublicKey, PskBundle<'a>),
}

// Helper function for setup_receiver
impl<'a, Kex: KeyExchange> OpModeR<'a, Kex> {
    /// Returns the sender's identity pubkey if it's specified
    pub(crate) fn get_pk_sender_id(&self) -> Option<&Kex::PublicKey> {
        match self {
            OpModeR::Auth(pk) => Some(pk),
            OpModeR::AuthPsk(pk, _) => Some(pk),
            _ => None,
        }
    }
}

/// The operation mode of the HPKE session (sender's view). This is how the sender authenticates
/// their identity to the receiver. This authentication information can include a preshared key,
/// the identity key of the sender, both, or neither. `Base` is the only mode that does not provide
/// any kind of sender identity authentication.
pub enum OpModeS<'a, Kex: KeyExchange> {
    /// No extra information included
    Base,
    /// A preshared key known to the sender and receiver
    Psk(PskBundle<'a>),
    /// The identity keypair of the sender
    Auth((Kex::PrivateKey, Kex::PublicKey)),
    /// Both of the above
    AuthPsk((Kex::PrivateKey, Kex::PublicKey), PskBundle<'a>),
}

// Helpers functions for setup_sender and testing
impl<'a, Kex: KeyExchange> OpModeS<'a, Kex> {
    /// Returns the sender's identity pubkey if it's specified
    pub(crate) fn get_sender_id_keypair(&self) -> Option<&(Kex::PrivateKey, Kex::PublicKey)> {
        match self {
            OpModeS::Auth(keypair) => Some(keypair),
            OpModeS::AuthPsk(keypair, _) => Some(keypair),
            _ => None,
        }
    }
}

/// Represents the convenience methods necessary for getting default values out of the operation
/// mode
pub(crate) trait OpMode<Kex: KeyExchange> {
    /// Gets the mode ID (hardcoded based on variant)
    fn mode_id(&self) -> u8;
    /// If this is a PSK mode, returns the PSK. Otherwise returns the empty string.
    fn get_psk_bytes(&self) -> &[u8];
    /// If this is a PSK mode, returns the PSK ID. Otherwise returns the empty string.
    fn get_psk_id(&self) -> &[u8];
}

impl<'a, Kex: KeyExchange> OpMode<Kex> for OpModeR<'a, Kex> {
    // Defined in draft02 §5.0
    fn mode_id(&self) -> u8 {
        match self {
            OpModeR::Base => 0x00,
            OpModeR::Psk(..) => 0x01,
            OpModeR::Auth(..) => 0x02,
            OpModeR::AuthPsk(..) => 0x03,
        }
    }

    // Returns the preshared key bytes if it's set in the mode, otherwise returns
    // [0u8; Kdf::HashImpl::OutputSize]
    fn get_psk_bytes(&self) -> &[u8] {
        // default_psk = zero(0)
        match self {
            OpModeR::Psk(bundle) => &bundle.psk,
            OpModeR::AuthPsk(_, bundle) => &bundle.psk,
            _ => &[],
        }
    }

    // Returns the preshared key ID if it's set in the mode, otherwise returns the emtpy string
    fn get_psk_id(&self) -> &[u8] {
        // default_pskID = zero(0)
        match self {
            OpModeR::Psk(p) => &p.psk_id,
            OpModeR::AuthPsk(_, p) => &p.psk_id,
            _ => &[],
        }
    }
}

// I know there's a bunch of code reuse here, but it's not so much that I feel the need to abstract
// something away
impl<'a, Kex: KeyExchange> OpMode<Kex> for OpModeS<'a, Kex> {
    // Defined in draft02 §5.0
    fn mode_id(&self) -> u8 {
        match self {
            OpModeS::Base => 0x00,
            OpModeS::Psk(..) => 0x01,
            OpModeS::Auth(..) => 0x02,
            OpModeS::AuthPsk(..) => 0x03,
        }
    }

    // Returns the preshared key bytes if it's set in the mode, otherwise returns
    // [0u8; Kdf::Hashfunction::OutputSize]
    fn get_psk_bytes(&self) -> &[u8] {
        // default_psk = zero(0)
        match self {
            OpModeS::Psk(bundle) => &bundle.psk,
            OpModeS::AuthPsk(_, bundle) => &bundle.psk,
            _ => &[],
        }
    }

    // Returns the preshared key ID if it's set in the mode, otherwise returns the emtpy string
    fn get_psk_id(&self) -> &[u8] {
        // default_pskID = zero(0)
        match self {
            OpModeS::Psk(p) => &p.psk_id,
            OpModeS::AuthPsk(_, p) => &p.psk_id,
            _ => &[],
        }
    }
}
