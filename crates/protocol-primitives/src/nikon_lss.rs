//! Nikon Linkage Setting Service authentication and encrypted-field primitives.
//!
//! This is a sans-I/O implementation. It produces and consumes the exact
//! 17-byte authentication records; callers remain responsible for GATT writes,
//! indications, timeouts, and disconnect policy. The session cipher is opaque,
//! redacted from `Debug`, and zeroized on drop by the underlying cipher type.

use std::fmt;

use blowfish::{
    cipher::{zeroize::Zeroize, Block, BlockCipherDecrypt, BlockCipherEncrypt, KeyInit},
    Blowfish,
};
use subtle::ConstantTimeEq;
use thiserror::Error;

const AUTH_RECORD_LENGTH: usize = 17;
const CIPHER_BLOCK_LENGTH: usize = 8;
const INITIAL_CHAIN: [u8; CIPHER_BLOCK_LENGTH] = [1, 2, 3, 4, 5, 6, 7, 8];

/// Fixed compatibility key used only by the LSS authentication proof.
///
/// This protocol constant is not a per-camera or per-session secret.
pub const NIKON_LSS_AUTHENTICATION_KEY: [u8; CIPHER_BLOCK_LENGTH] =
    [0xff, 0xff, 0xaa, 0x55, 0x11, 0x22, 0x33, 0x00];

/// The eight independently reconstructed authentication-table selections.
///
/// Each row is one 64-bit input block represented as two big-endian words.
pub const NIKON_LSS_AUTHENTICATION_TABLE: [[u32; 2]; 8] = [
    [0x7040_66e4, 0x0433_d552],
    [0xed4b_8fac, 0x15f7_e47b],
    [0x2447_1f11, 0x8b5e_a1fc],
    [0x0596_0c31, 0x2b8c_7f41],
    [0xfda5_88c1, 0xeba8_b1f3],
    [0x9916_6056, 0x1bd3_d550],
    [0xcd32_687f, 0xa9e2_8a30],
    [0x2a8f_e834, 0xdec7_ebf4],
];

/// One of the eight authentication-table rows selected by the responder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NikonLssAuthenticationSelection(u8);

impl NikonLssAuthenticationSelection {
    /// Construct a selection from its stable table index (`0..=7`).
    pub fn new(index: u8) -> Result<Self, NikonLssError> {
        if usize::from(index) < NIKON_LSS_AUTHENTICATION_TABLE.len() {
            Ok(Self(index))
        } else {
            Err(NikonLssError::InvalidAuthenticationSelection(index))
        }
    }

    /// Return the stable table index.
    pub const fn index(self) -> u8 {
        self.0
    }
}

/// Failures from LSS record processing, authentication, or configuration decode.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum NikonLssError {
    #[error("LSS authentication record must be exactly 17 bytes, got {actual}")]
    InvalidRecordLength { actual: usize },
    #[error("expected LSS authentication stage {expected}, got {actual}")]
    UnexpectedStage { expected: u8, actual: u8 },
    #[error("LSS authentication call is invalid in state {state}")]
    InvalidState { state: &'static str },
    #[error("LSS authentication proof did not match any compatibility-table selection")]
    AuthenticationFailed,
    #[error("LSS authentication-table selection {0} is outside 0..=7")]
    InvalidAuthenticationSelection(u8),
    #[error("LSS cipher input length {actual} is not a multiple of 8")]
    InvalidCipherLength { actual: usize },
    #[error("LSS connection configuration is missing {field}: need {needed} bytes, have {actual}")]
    ConfigurationTruncated {
        field: &'static str,
        needed: usize,
        actual: usize,
    },
    #[error("LSS connection configuration {field} contains non-NUL data after its terminator")]
    InvalidStringPadding { field: &'static str },
    #[error("LSS connection configuration {field} is not UTF-8")]
    InvalidStringEncoding { field: &'static str },
    #[error("unknown Nikon Wi-Fi security mode {0}")]
    UnknownWifiSecurity(u8),
    #[error("LSS connection configuration has {0} unexpected trailing bytes")]
    ConfigurationTrailingBytes(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClientState {
    Ready,
    AwaitingStage2,
    AwaitingStage4,
}

/// Initiator side of the four-record LSS authentication exchange.
///
/// The client device id is persistent pairing identity. `client_nonce` must be
/// fresh runtime entropy for each authentication attempt. Neither value is
/// exposed after construction.
pub struct NikonLssClient {
    state: ClientState,
    client_device_id: [u8; 8],
    client_nonce: [u8; 8],
    camera_nonce: [u8; 8],
    selection: Option<NikonLssAuthenticationSelection>,
}

impl fmt::Debug for NikonLssClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NikonLssClient")
            .field("state", &self.state)
            .field("authentication_material", &"[REDACTED]")
            .finish()
    }
}

impl Drop for NikonLssClient {
    fn drop(&mut self) {
        self.client_device_id.zeroize();
        self.client_nonce.zeroize();
        self.camera_nonce.zeroize();
    }
}

impl NikonLssClient {
    /// Start an initiator exchange with a persistent id and fresh nonce.
    pub fn new(client_device_id: [u8; 8], client_nonce: [u8; 8]) -> Self {
        Self {
            state: ClientState::Ready,
            client_device_id,
            client_nonce,
            camera_nonce: [0; 8],
            selection: None,
        }
    }

    /// Produce stage 1: `[0x01][client nonce][persistent client device id]`.
    pub fn stage1_record(&mut self) -> Result<[u8; AUTH_RECORD_LENGTH], NikonLssError> {
        if self.state != ClientState::Ready {
            return Err(NikonLssError::InvalidState {
                state: client_state_name(self.state),
            });
        }
        self.state = ClientState::AwaitingStage2;
        Ok(pack_record(1, self.client_nonce, self.client_device_id))
    }

    /// Validate stage 2 and produce the matching stage-3 proof record.
    pub fn handle_stage2(
        &mut self,
        record: &[u8],
    ) -> Result<[u8; AUTH_RECORD_LENGTH], NikonLssError> {
        if self.state != ClientState::AwaitingStage2 {
            return Err(NikonLssError::InvalidState {
                state: client_state_name(self.state),
            });
        }
        let (_, camera_nonce, camera_proof) = unpack_record(record, 2)?;
        let selection = (0..NIKON_LSS_AUTHENTICATION_TABLE.len())
            .map(|index| NikonLssAuthenticationSelection(index as u8))
            .find(|selection| {
                bool::from(
                    authentication_proof(*selection, camera_nonce, self.client_nonce)
                        .ct_eq(&camera_proof),
                )
            })
            .ok_or(NikonLssError::AuthenticationFailed)?;
        let client_proof = authentication_proof(selection, self.client_nonce, camera_nonce);

        self.camera_nonce = camera_nonce;
        self.selection = Some(selection);
        self.state = ClientState::AwaitingStage4;
        Ok(pack_record(3, self.client_nonce, client_proof))
    }

    /// Validate stage 4 and consume the exchange into an opaque cipher session.
    pub fn finish_stage4(self, record: &[u8]) -> Result<NikonLssSession, NikonLssError> {
        if self.state != ClientState::AwaitingStage4 {
            return Err(NikonLssError::InvalidState {
                state: client_state_name(self.state),
            });
        }
        let (_, _, server_device_id) = unpack_record(record, 4)?;
        let selection = self.selection.ok_or(NikonLssError::InvalidState {
            state: client_state_name(self.state),
        })?;
        Ok(derive_session(
            selection,
            self.client_nonce,
            self.camera_nonce,
            self.client_device_id,
            server_device_id,
        ))
    }

    /// Return the authenticated table selection after stage 2.
    pub fn authentication_selection(&self) -> Option<NikonLssAuthenticationSelection> {
        self.selection
    }
}

fn client_state_name(state: ClientState) -> &'static str {
    match state {
        ClientState::Ready => "ready",
        ClientState::AwaitingStage2 => "awaiting-stage-2",
        ClientState::AwaitingStage4 => "awaiting-stage-4",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServerState {
    AwaitingStage1,
    AwaitingStage3,
    Complete,
}

/// Responder side of the LSS authentication exchange.
///
/// `camera_nonce` must be fresh runtime entropy; `server_device_id` is the
/// responder's persistent identity. The caller chooses an authentication-table
/// row so tests and simulators can cover every compatible selection without
/// embedding model policy in this crate.
pub struct NikonLssServer {
    state: ServerState,
    selection: NikonLssAuthenticationSelection,
    camera_nonce: [u8; 8],
    server_device_id: [u8; 8],
    client_nonce: [u8; 8],
    client_device_id: [u8; 8],
}

impl fmt::Debug for NikonLssServer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NikonLssServer")
            .field("state", &self.state)
            .field("authentication_material", &"[REDACTED]")
            .finish()
    }
}

impl Drop for NikonLssServer {
    fn drop(&mut self) {
        self.camera_nonce.zeroize();
        self.server_device_id.zeroize();
        self.client_nonce.zeroize();
        self.client_device_id.zeroize();
    }
}

impl NikonLssServer {
    /// Start a responder exchange with fresh runtime entropy and a persistent id.
    pub fn new(
        selection: NikonLssAuthenticationSelection,
        camera_nonce: [u8; 8],
        server_device_id: [u8; 8],
    ) -> Self {
        Self {
            state: ServerState::AwaitingStage1,
            selection,
            camera_nonce,
            server_device_id,
            client_nonce: [0; 8],
            client_device_id: [0; 8],
        }
    }

    /// Validate stage 1 and produce stage 2.
    pub fn handle_stage1(
        &mut self,
        record: &[u8],
    ) -> Result<[u8; AUTH_RECORD_LENGTH], NikonLssError> {
        if self.state != ServerState::AwaitingStage1 {
            return Err(NikonLssError::InvalidState {
                state: server_state_name(self.state),
            });
        }
        let (_, client_nonce, client_device_id) = unpack_record(record, 1)?;
        let camera_proof = authentication_proof(self.selection, self.camera_nonce, client_nonce);
        self.client_nonce = client_nonce;
        self.client_device_id = client_device_id;
        self.state = ServerState::AwaitingStage3;
        Ok(pack_record(2, self.camera_nonce, camera_proof))
    }

    /// Validate stage 3 and produce stage 4 plus an opaque session.
    ///
    /// A proof mismatch leaves the responder in the stage-3 state so the
    /// caller can retry with a corrected record, matching the native state
    /// machine.
    pub fn finish_stage3(
        &mut self,
        record: &[u8],
    ) -> Result<([u8; AUTH_RECORD_LENGTH], NikonLssSession), NikonLssError> {
        if self.state != ServerState::AwaitingStage3 {
            return Err(NikonLssError::InvalidState {
                state: server_state_name(self.state),
            });
        }
        let (_, echoed_client_nonce, client_proof) = unpack_record(record, 3)?;
        let expected = authentication_proof(self.selection, self.client_nonce, self.camera_nonce);
        if echoed_client_nonce != self.client_nonce || !bool::from(client_proof.ct_eq(&expected)) {
            return Err(NikonLssError::AuthenticationFailed);
        }

        let session = derive_session(
            self.selection,
            self.client_nonce,
            self.camera_nonce,
            self.client_device_id,
            self.server_device_id,
        );
        let stage4 = pack_record(4, self.camera_nonce, self.server_device_id);
        self.state = ServerState::Complete;
        Ok((stage4, session))
    }
}

fn server_state_name(state: ServerState) -> &'static str {
    match state {
        ServerState::AwaitingStage1 => "awaiting-stage-1",
        ServerState::AwaitingStage3 => "awaiting-stage-3",
        ServerState::Complete => "complete",
    }
}

/// Opaque, zeroized LSS session cipher.
pub struct NikonLssSession {
    cipher: Blowfish,
    restore: NikonLssRestoreMaterial,
}

impl fmt::Debug for NikonLssSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NikonLssSession")
            .field("cipher", &"[REDACTED]")
            .field("restore_material", &"[REDACTED]")
            .finish()
    }
}

/// Opaque in-process checkpoint for restoring an LSS session.
///
/// This contains only compact semantic inputs. It is intentionally neither
/// serializable nor available as bytes, and regenerates the expanded schedule
/// when restored. Dropping it zeroizes the retained nonces and identities.
pub struct NikonLssContext {
    version: u8,
    restore: NikonLssRestoreMaterial,
}

#[derive(Clone)]
struct NikonLssRestoreMaterial {
    selection: NikonLssAuthenticationSelection,
    client_nonce: [u8; 8],
    camera_nonce: [u8; 8],
    client_device_id: [u8; 8],
    server_device_id: [u8; 8],
}

impl Drop for NikonLssRestoreMaterial {
    fn drop(&mut self) {
        self.client_nonce.zeroize();
        self.camera_nonce.zeroize();
        self.client_device_id.zeroize();
        self.server_device_id.zeroize();
    }
}

impl fmt::Debug for NikonLssContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NikonLssContext")
            .field("version", &self.version)
            .field("restore_material", &"[REDACTED]")
            .finish()
    }
}

impl NikonLssSession {
    /// Encrypt a whole number of 8-byte blocks with the LSS zero-IV chaining.
    pub fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>, NikonLssError> {
        validate_cipher_length(plaintext)?;
        let mut output = Vec::with_capacity(plaintext.len());
        let mut previous = [0u8; CIPHER_BLOCK_LENGTH];
        for chunk in plaintext.chunks_exact(CIPHER_BLOCK_LENGTH) {
            let mut bytes = [0u8; CIPHER_BLOCK_LENGTH];
            bytes.copy_from_slice(chunk);
            for (byte, prior) in bytes.iter_mut().zip(previous) {
                *byte ^= prior;
            }
            let mut block = Block::<Blowfish>::from(bytes);
            self.cipher.encrypt_block(&mut block);
            previous.copy_from_slice(&block);
            output.extend_from_slice(&block);
            bytes.zeroize();
        }
        previous.zeroize();
        Ok(output)
    }

    /// Decrypt a whole number of 8-byte blocks with the LSS zero-IV chaining.
    pub fn decrypt(&self, ciphertext: &[u8]) -> Result<Vec<u8>, NikonLssError> {
        validate_cipher_length(ciphertext)?;
        let mut output = Vec::with_capacity(ciphertext.len());
        let mut previous = [0u8; CIPHER_BLOCK_LENGTH];
        for chunk in ciphertext.chunks_exact(CIPHER_BLOCK_LENGTH) {
            let mut encrypted = [0u8; CIPHER_BLOCK_LENGTH];
            encrypted.copy_from_slice(chunk);
            let mut block = Block::<Blowfish>::from(encrypted);
            self.cipher.decrypt_block(&mut block);
            for (byte, prior) in block.iter_mut().zip(previous) {
                *byte ^= prior;
            }
            output.extend_from_slice(&block);
            previous = encrypted;
            block.zeroize();
            encrypted.zeroize();
        }
        previous.zeroize();
        Ok(output)
    }

    /// Clone compact semantic inputs for an opaque in-process restore point.
    pub fn checkpoint(&self) -> NikonLssContext {
        NikonLssContext {
            version: 1,
            restore: self.restore.clone(),
        }
    }

    /// Restore a session by regenerating its schedule from semantic inputs.
    pub fn restore(context: NikonLssContext) -> Self {
        debug_assert_eq!(context.version, 1);
        derive_session(
            context.restore.selection,
            context.restore.client_nonce,
            context.restore.camera_nonce,
            context.restore.client_device_id,
            context.restore.server_device_id,
        )
    }

    /// Decode the single LSS connection-configuration characteristic value.
    pub fn decode_connection_configuration(
        &self,
        value: &[u8],
    ) -> Result<NikonConnectionConfiguration, NikonLssError> {
        let (&flags, mut remaining) =
            value
                .split_first()
                .ok_or(NikonLssError::ConfigurationTruncated {
                    field: "flags",
                    needed: 1,
                    actual: 0,
                })?;

        let wifi = if flags & 0x01 != 0 {
            let (encrypted_ssid, tail) = take_configuration(remaining, 32, "Wi-Fi SSID")?;
            remaining = tail;
            let (encrypted_password, tail) = take_configuration(remaining, 64, "Wi-Fi password")?;
            remaining = tail;
            let (&security, tail) =
                remaining
                    .split_first()
                    .ok_or(NikonLssError::ConfigurationTruncated {
                        field: "Wi-Fi security mode",
                        needed: 1,
                        actual: 0,
                    })?;
            remaining = tail;

            let ssid = decode_nul_padded(self.decrypt(encrypted_ssid)?, "Wi-Fi SSID")?;
            let password = decode_nul_padded(self.decrypt(encrypted_password)?, "Wi-Fi password")?;
            Some(NikonWifiConfiguration {
                ssid,
                password,
                security: NikonWifiSecurity::try_from(security)?,
            })
        } else {
            None
        };

        let spp_maximum_length = if flags & 0x02 != 0 {
            let (bytes, tail) = take_configuration(remaining, 4, "SPP maximum length")?;
            remaining = tail;
            Some(u32::from_le_bytes(
                bytes.try_into().expect("length checked"),
            ))
        } else {
            None
        };

        if !remaining.is_empty() {
            return Err(NikonLssError::ConfigurationTrailingBytes(remaining.len()));
        }
        Ok(NikonConnectionConfiguration {
            flags,
            wifi,
            spp_maximum_length,
        })
    }
}

fn validate_cipher_length(value: &[u8]) -> Result<(), NikonLssError> {
    if value.len().is_multiple_of(CIPHER_BLOCK_LENGTH) {
        Ok(())
    } else {
        Err(NikonLssError::InvalidCipherLength {
            actual: value.len(),
        })
    }
}

fn take_configuration<'a>(
    value: &'a [u8],
    length: usize,
    field: &'static str,
) -> Result<(&'a [u8], &'a [u8]), NikonLssError> {
    if value.len() < length {
        return Err(NikonLssError::ConfigurationTruncated {
            field,
            needed: length,
            actual: value.len(),
        });
    }
    Ok(value.split_at(length))
}

fn decode_nul_padded(mut value: Vec<u8>, field: &'static str) -> Result<String, NikonLssError> {
    let string_length = value
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(value.len());
    if value[string_length..].iter().any(|byte| *byte != 0) {
        value.zeroize();
        return Err(NikonLssError::InvalidStringPadding { field });
    }
    let result = String::from_utf8(value[..string_length].to_vec())
        .map_err(|_| NikonLssError::InvalidStringEncoding { field });
    value.zeroize();
    result
}

/// Parsed contents of the single connection-configuration characteristic read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NikonConnectionConfiguration {
    pub flags: u8,
    pub wifi: Option<NikonWifiConfiguration>,
    pub spp_maximum_length: Option<u32>,
}

/// Decrypted Wi-Fi handoff fields.
#[derive(Clone, PartialEq, Eq)]
pub struct NikonWifiConfiguration {
    pub ssid: String,
    pub password: String,
    pub security: NikonWifiSecurity,
}

impl fmt::Debug for NikonWifiConfiguration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NikonWifiConfiguration")
            .field("ssid", &self.ssid)
            .field("password", &"[REDACTED]")
            .field("security", &self.security)
            .finish()
    }
}

/// SnapBridge connection-configuration Wi-Fi security byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NikonWifiSecurity {
    Open,
    Wpa2,
    Wpa3,
    Wpa2Wpa3,
}

impl NikonWifiSecurity {
    /// Stable manifest-scope token for the security mode.
    pub const fn as_token(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Wpa2 => "wpa2",
            Self::Wpa3 => "wpa3",
            Self::Wpa2Wpa3 => "wpa2Wpa3",
        }
    }
}

impl TryFrom<u8> for NikonWifiSecurity {
    type Error = NikonLssError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Open),
            1 => Ok(Self::Wpa2),
            2 => Ok(Self::Wpa3),
            3 => Ok(Self::Wpa2Wpa3),
            _ => Err(NikonLssError::UnknownWifiSecurity(value)),
        }
    }
}

fn pack_record(stage: u8, first: [u8; 8], second: [u8; 8]) -> [u8; AUTH_RECORD_LENGTH] {
    let mut record = [0u8; AUTH_RECORD_LENGTH];
    record[0] = stage;
    record[1..9].copy_from_slice(&first);
    record[9..17].copy_from_slice(&second);
    record
}

fn unpack_record(
    record: &[u8],
    expected_stage: u8,
) -> Result<(u8, [u8; 8], [u8; 8]), NikonLssError> {
    if record.len() != AUTH_RECORD_LENGTH {
        return Err(NikonLssError::InvalidRecordLength {
            actual: record.len(),
        });
    }
    if record[0] != expected_stage {
        return Err(NikonLssError::UnexpectedStage {
            expected: expected_stage,
            actual: record[0],
        });
    }
    let first = record[1..9].try_into().expect("length checked");
    let second = record[9..17].try_into().expect("length checked");
    Ok((record[0], first, second))
}

fn authentication_proof(
    selection: NikonLssAuthenticationSelection,
    first: [u8; 8],
    second: [u8; 8],
) -> [u8; 8] {
    let words = NIKON_LSS_AUTHENTICATION_TABLE[usize::from(selection.0)];
    let mut table_block = [0u8; 8];
    table_block[..4].copy_from_slice(&words[0].to_be_bytes());
    table_block[4..].copy_from_slice(&words[1].to_be_bytes());
    authentication_hash(&[table_block, first, second])
}

fn authentication_hash(blocks: &[[u8; 8]]) -> [u8; 8] {
    let cipher: Blowfish = Blowfish::new_from_slice(&NIKON_LSS_AUTHENTICATION_KEY)
        .expect("the fixed LSS authentication key has a valid length");
    let mut chain = INITIAL_CHAIN;
    for input in blocks {
        for (byte, input_byte) in chain.iter_mut().zip(input) {
            *byte ^= input_byte;
        }
        let mut block = Block::<Blowfish>::from(chain);
        cipher.encrypt_block(&mut block);
        chain.copy_from_slice(&block);
    }
    chain
}

fn derive_session(
    selection: NikonLssAuthenticationSelection,
    mut client_nonce: [u8; 8],
    mut camera_nonce: [u8; 8],
    mut client_device_id: [u8; 8],
    mut server_device_id: [u8; 8],
) -> NikonLssSession {
    let restore = NikonLssRestoreMaterial {
        selection,
        client_nonce,
        camera_nonce,
        client_device_id,
        server_device_id,
    };
    let mut summary = [0u8; 8];
    summary[0] = selection.index();
    summary[1..4].copy_from_slice(&camera_nonce[1..4]);
    summary[4..].copy_from_slice(&client_nonce[..4]);
    let mut seed = authentication_hash(&[server_device_id, client_device_id, summary]);
    let cipher =
        Blowfish::new_from_slice(&seed).expect("the derived LSS session key has a valid length");
    client_nonce.zeroize();
    camera_nonce.zeroize();
    client_device_id.zeroize();
    server_device_id.zeroize();
    summary.zeroize();
    seed.zeroize();
    NikonLssSession { cipher, restore }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLIENT_NONCE: [u8; 8] = [0x01, 0x23, 0x45, 0x67, 0x11, 0x22, 0x33, 0x44];
    const SERVER_NONCE: [u8; 8] = [0x55, 0x66, 0x77, 0x00, 0x01, 0x02, 0x03, 0x04];
    const CLIENT_ID: [u8; 8] = [0xc0, 0xc1, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7];
    const SERVER_ID: [u8; 8] = [0xd0, 0xd1, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7];
    const ORACLE_PROOF_2: [[u8; 8]; 8] = [
        [0xce, 0xbe, 0x86, 0x0c, 0xad, 0xb1, 0x0d, 0x0f],
        [0x02, 0x94, 0x6b, 0xdc, 0xe3, 0x68, 0xfa, 0x14],
        [0xef, 0x9c, 0x01, 0x5a, 0x73, 0x8a, 0x61, 0xcd],
        [0x51, 0x7a, 0x83, 0x63, 0xad, 0xe8, 0x21, 0xa9],
        [0x0b, 0x2b, 0x84, 0xb0, 0x7e, 0x7b, 0x92, 0xfe],
        [0x3a, 0x83, 0x9a, 0xba, 0x84, 0xaf, 0xa9, 0xd8],
        [0x71, 0x8e, 0x8f, 0x7d, 0x28, 0x31, 0xe2, 0x6e],
        [0xc3, 0x9d, 0x08, 0xc2, 0x79, 0xda, 0x30, 0x80],
    ];
    const ORACLE_PROOF_3: [[u8; 8]; 8] = [
        [0xd4, 0x5c, 0x8b, 0xee, 0xd3, 0xd5, 0x5c, 0x37],
        [0x16, 0x27, 0xd6, 0x21, 0x3d, 0x9a, 0x90, 0x23],
        [0x91, 0x03, 0xa6, 0xbf, 0x46, 0x19, 0x90, 0x80],
        [0x83, 0xcf, 0x87, 0xde, 0x7f, 0xba, 0x4a, 0x18],
        [0xfc, 0x1e, 0xb2, 0x93, 0xef, 0x10, 0x18, 0x2e],
        [0x11, 0x7b, 0x0d, 0x45, 0x42, 0x1e, 0x8f, 0xa9],
        [0x82, 0x6b, 0x84, 0x2d, 0x58, 0xc0, 0x89, 0x50],
        [0xf2, 0x07, 0xc9, 0x0c, 0xd6, 0xee, 0x68, 0x5d],
    ];
    const ORACLE_CIPHERTEXT_32: [u8; 32] = [
        0x60, 0x98, 0xfd, 0x9a, 0xaa, 0x72, 0x26, 0x5a, 0xac, 0x47, 0x5b, 0x61, 0xfd, 0xb4, 0x79,
        0xf7, 0xcd, 0x9c, 0x82, 0x32, 0x9d, 0x1b, 0x2d, 0xea, 0xfe, 0x90, 0xaa, 0x5f, 0x5a, 0x39,
        0x6c, 0x43,
    ];

    fn establish(selection: u8) -> (NikonLssSession, NikonLssSession) {
        let selection = NikonLssAuthenticationSelection::new(selection).unwrap();
        let mut client = NikonLssClient::new(CLIENT_ID, CLIENT_NONCE);
        let mut server = NikonLssServer::new(selection, SERVER_NONCE, SERVER_ID);
        let stage1 = client.stage1_record().unwrap();
        let stage2 = server.handle_stage1(&stage1).unwrap();
        let stage3 = client.handle_stage2(&stage2).unwrap();
        let (stage4, server_session) = server.finish_stage3(&stage3).unwrap();
        let client_session = client.finish_stage4(&stage4).unwrap();
        (client_session, server_session)
    }

    #[test]
    fn matches_native_oracle_records_for_both_roles_and_all_eight_selections() {
        for selection_index in 0..8 {
            let selection = NikonLssAuthenticationSelection::new(selection_index).unwrap();
            let mut client = NikonLssClient::new(CLIENT_ID, CLIENT_NONCE);
            let mut server = NikonLssServer::new(selection, SERVER_NONCE, SERVER_ID);

            let stage1 = client.stage1_record().unwrap();
            assert_eq!(stage1, pack_record(1, CLIENT_NONCE, CLIENT_ID));
            let stage2 = server.handle_stage1(&stage1).unwrap();
            assert_eq!(
                stage2,
                pack_record(2, SERVER_NONCE, ORACLE_PROOF_2[selection_index as usize])
            );
            let stage3 = client.handle_stage2(&stage2).unwrap();
            assert_eq!(client.authentication_selection(), Some(selection));
            assert_eq!(
                stage3,
                pack_record(3, CLIENT_NONCE, ORACLE_PROOF_3[selection_index as usize])
            );
            let (stage4, server_session) = server.finish_stage3(&stage3).unwrap();
            assert_eq!(stage4, pack_record(4, SERVER_NONCE, SERVER_ID));
            let client_session = client.finish_stage4(&stage4).unwrap();

            let plaintext = *b"0123456789ABCDEF0123456789ABCDEF";
            let ciphertext = client_session.encrypt(&plaintext).unwrap();
            assert_ne!(ciphertext, plaintext, "selection {selection_index}");
            assert_eq!(server_session.decrypt(&ciphertext).unwrap(), plaintext);
        }
    }

    #[test]
    fn matches_independently_published_authentication_vector() {
        let client_nonce = 0x677d_a144_ec13_e1dbu64.to_le_bytes();
        let camera_nonce = 0xb994_3d5e_8026_fa29u64.to_le_bytes();
        let camera_proof = [0xe4, 0xf2, 0xb3, 0xa8, 0x13, 0x6a, 0xd5, 0x16];
        let expected_client_proof = [0x53, 0xad, 0xf1, 0x79, 0x35, 0x8a, 0x83, 0x23];
        let mut client = NikonLssClient::new(CLIENT_ID, client_nonce);
        client.stage1_record().unwrap();
        let stage3 = client
            .handle_stage2(&pack_record(2, camera_nonce, camera_proof))
            .unwrap();
        assert_eq!(client.authentication_selection().unwrap().index(), 6);
        assert_eq!(&stage3[1..9], &client_nonce);
        assert_eq!(&stage3[9..17], &expected_client_proof);
    }

    #[test]
    fn rejects_mutated_proofs_and_illegal_state_transitions() {
        let selection = NikonLssAuthenticationSelection::new(3).unwrap();
        let mut client = NikonLssClient::new(CLIENT_ID, CLIENT_NONCE);
        let mut server = NikonLssServer::new(selection, SERVER_NONCE, SERVER_ID);
        assert!(matches!(
            client.handle_stage2(&[0; 17]),
            Err(NikonLssError::InvalidState { .. })
        ));
        let stage1 = client.stage1_record().unwrap();
        assert!(matches!(
            client.stage1_record(),
            Err(NikonLssError::InvalidState { .. })
        ));
        let mut stage2 = server.handle_stage1(&stage1).unwrap();
        let valid_stage2 = stage2;
        stage2[9] ^= 0x01;
        assert_eq!(
            client.handle_stage2(&stage2),
            Err(NikonLssError::AuthenticationFailed)
        );
        let stage3 = client.handle_stage2(&valid_stage2).unwrap();
        assert_eq!(stage3[9..], ORACLE_PROOF_3[3]);

        let mut client = NikonLssClient::new(CLIENT_ID, CLIENT_NONCE);
        let mut server = NikonLssServer::new(selection, SERVER_NONCE, SERVER_ID);
        let stage1 = client.stage1_record().unwrap();
        let stage2 = server.handle_stage1(&stage1).unwrap();
        let mut stage3 = client.handle_stage2(&stage2).unwrap();
        let valid_stage3 = stage3;
        stage3[16] ^= 0x01;
        assert!(matches!(
            server.finish_stage3(&stage3),
            Err(NikonLssError::AuthenticationFailed)
        ));
        let (stage4, _) = server.finish_stage3(&valid_stage3).unwrap();
        assert_eq!(stage4, pack_record(4, SERVER_NONCE, SERVER_ID));
        assert!(matches!(
            server.finish_stage3(&valid_stage3),
            Err(NikonLssError::InvalidState { .. })
        ));
    }

    #[test]
    fn rejects_malformed_record_and_cipher_lengths() {
        assert_eq!(
            NikonLssAuthenticationSelection::new(8),
            Err(NikonLssError::InvalidAuthenticationSelection(8))
        );
        let mut client = NikonLssClient::new(CLIENT_ID, CLIENT_NONCE);
        client.stage1_record().unwrap();
        assert_eq!(
            client.handle_stage2(&[2; 16]),
            Err(NikonLssError::InvalidRecordLength { actual: 16 })
        );
        let (session, _) = establish(0);
        assert_eq!(
            session.decrypt(&[0; 7]),
            Err(NikonLssError::InvalidCipherLength { actual: 7 })
        );
        assert_eq!(session.encrypt(&[]).unwrap(), Vec::<u8>::new());
        assert_eq!(session.decrypt(&[]).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn encrypts_and_decrypts_32_and_64_byte_payloads() {
        let (session, _) = establish(7);
        for length in [32, 64] {
            let plaintext = (0..length).map(|byte| byte as u8).collect::<Vec<_>>();
            let ciphertext = session.encrypt(&plaintext).unwrap();
            assert_eq!(ciphertext.len(), length);
            assert_eq!(session.decrypt(&ciphertext).unwrap(), plaintext);
        }
    }

    #[test]
    fn matches_native_oracle_cipher_vectors_for_32_and_64_bytes() {
        let (session, _) = establish(0);
        let plaintext32 = (0..32).map(|byte| byte as u8).collect::<Vec<_>>();
        assert_eq!(session.encrypt(&plaintext32).unwrap(), ORACLE_CIPHERTEXT_32);
        assert_eq!(session.decrypt(&ORACLE_CIPHERTEXT_32).unwrap(), plaintext32);

        let plaintext64 = (0x80..=0xbf).collect::<Vec<u8>>();
        let ciphertext64 = [
            0x84, 0x73, 0x57, 0x35, 0xf2, 0xd5, 0xa8, 0x0b, 0x9a, 0xb2, 0x65, 0x05, 0x9a, 0x87,
            0xa7, 0xd3, 0xb3, 0xd9, 0xbe, 0x57, 0xd5, 0x27, 0xc5, 0xab, 0xa4, 0x02, 0x35, 0x8b,
            0x4a, 0xee, 0x06, 0x5c, 0xfc, 0xae, 0x3d, 0x59, 0xb0, 0xc0, 0xf5, 0xf6, 0x81, 0xe6,
            0xf3, 0xcb, 0x72, 0xec, 0xc7, 0x50, 0x65, 0xd7, 0x45, 0xcb, 0x76, 0xf5, 0x7d, 0x3f,
            0x33, 0xac, 0x85, 0x2f, 0x0a, 0x3d, 0xe7, 0x20,
        ];
        assert_eq!(session.encrypt(&plaintext64).unwrap(), ciphertext64);
        assert_eq!(session.decrypt(&ciphertext64).unwrap(), plaintext64);
    }

    #[test]
    fn mutated_server_device_id_derives_a_different_session() {
        let selection = NikonLssAuthenticationSelection::new(4).unwrap();
        let mut client = NikonLssClient::new(CLIENT_ID, CLIENT_NONCE);
        let mut server = NikonLssServer::new(selection, SERVER_NONCE, SERVER_ID);
        let stage1 = client.stage1_record().unwrap();
        let stage2 = server.handle_stage1(&stage1).unwrap();
        let stage3 = client.handle_stage2(&stage2).unwrap();
        let (mut stage4, server_session) = server.finish_stage3(&stage3).unwrap();
        stage4[16] ^= 1;
        let mutated_client_session = client.finish_stage4(&stage4).unwrap();
        let plaintext = *b"stage 4 changes!";
        let ciphertext = server_session.encrypt(&plaintext).unwrap();
        assert_ne!(
            mutated_client_session.decrypt(&ciphertext).unwrap(),
            plaintext
        );
    }

    #[test]
    fn client_ignores_stage4_nonce_but_uses_server_device_id() {
        let selection = NikonLssAuthenticationSelection::new(4).unwrap();
        let mut client = NikonLssClient::new(CLIENT_ID, CLIENT_NONCE);
        let mut server = NikonLssServer::new(selection, SERVER_NONCE, SERVER_ID);
        let stage1 = client.stage1_record().unwrap();
        let stage2 = server.handle_stage1(&stage1).unwrap();
        let stage3 = client.handle_stage2(&stage2).unwrap();
        let (mut stage4, server_session) = server.finish_stage3(&stage3).unwrap();
        stage4[1] ^= 0xff;
        let client_session = client.finish_stage4(&stage4).unwrap();
        let plaintext = *b"nonce is ignored";
        assert_eq!(
            client_session.encrypt(&plaintext).unwrap(),
            server_session.encrypt(&plaintext).unwrap()
        );
    }

    #[test]
    fn opaque_context_restores_cipher_compatibility_and_redacts_debug() {
        let (session, _) = establish(0);
        let context = session.checkpoint();
        assert!(format!("{session:?}").contains("[REDACTED]"));
        assert!(format!("{context:?}").contains("[REDACTED]"));
        drop(session);
        let restored = NikonLssSession::restore(context);
        let plaintext = (0..32).map(|byte| byte as u8).collect::<Vec<_>>();
        assert_eq!(restored.encrypt(&plaintext).unwrap(), ORACLE_CIPHERTEXT_32);
        assert_eq!(restored.decrypt(&ORACLE_CIPHERTEXT_32).unwrap(), plaintext);
    }

    #[test]
    fn decodes_wifi_and_spp_configuration_from_one_read() {
        let (session, _) = establish(5);
        let mut ssid = [0u8; 32];
        ssid[..10].copy_from_slice(b"NIKON_D850");
        let mut password = [0u8; 64];
        password[..12].copy_from_slice(b"snap-bridge!");
        let mut value = vec![0x03];
        value.extend(session.encrypt(&ssid).unwrap());
        value.extend(session.encrypt(&password).unwrap());
        value.push(1);
        value.extend_from_slice(&512u32.to_le_bytes());

        assert_eq!(
            session.decode_connection_configuration(&value).unwrap(),
            NikonConnectionConfiguration {
                flags: 3,
                wifi: Some(NikonWifiConfiguration {
                    ssid: "NIKON_D850".into(),
                    password: "snap-bridge!".into(),
                    security: NikonWifiSecurity::Wpa2,
                }),
                spp_maximum_length: Some(512),
            }
        );
        assert_eq!(NikonWifiSecurity::Wpa2.as_token(), "wpa2");
    }

    #[test]
    fn rejects_truncated_and_unknown_configuration_fields() {
        let (session, _) = establish(2);
        assert!(matches!(
            session.decode_connection_configuration(&[1, 0]),
            Err(NikonLssError::ConfigurationTruncated { .. })
        ));

        let mut value = vec![1];
        value.extend(session.encrypt(&[0; 32]).unwrap());
        value.extend(session.encrypt(&[0; 64]).unwrap());
        value.push(9);
        assert_eq!(
            session.decode_connection_configuration(&value),
            Err(NikonLssError::UnknownWifiSecurity(9))
        );
    }

    #[test]
    fn debug_output_never_contains_authentication_values() {
        let client = NikonLssClient::new(CLIENT_ID, CLIENT_NONCE);
        let debug = format!("{client:?}");
        assert!(!debug.contains("10, 32, 54"));
        assert!(!debug.contains("01, 23, 45"));
        assert!(debug.contains("[REDACTED]"));

        let config = NikonWifiConfiguration {
            ssid: "NIKON_D850".into(),
            password: "never-print-this".into(),
            security: NikonWifiSecurity::Wpa2,
        };
        let debug = format!("{config:?}");
        assert!(!debug.contains("never-print-this"));
        assert!(debug.contains("[REDACTED]"));
    }
}
