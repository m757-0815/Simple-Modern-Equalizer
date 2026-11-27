use std::fs;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::ptr::null;
use std::time::Duration;
use subtle::ConstantTimeEq;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Cryptography::{
    BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom, CRYPT_INTEGER_BLOB,
    CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};

// #######################################################
// publish the loopback endpoint under the current user
// #######################################################

pub type Token = [u8; 32];
pub const ACCEPTED: u8 = 1;
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_millis(500);

/// Publish a fresh credential with the port. Windows DPAPI binds the whole
/// record to the current user so a shared port-file path cannot expose it.
///
/// # Errors
/// Returns an error if Windows cannot generate or protect the credential, or
/// if the endpoint file cannot be written.
pub fn publish(path: &Path, port: u16) -> io::Result<Token> {
    let mut token = Token::default();
    // SAFETY: the system RNG writes at most the length of the supplied array.
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            token.as_mut_ptr(),
            token.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status < 0 {
        return Err(io::Error::other(format!("Windows RNG failed: {status:#x}")));
    }

    let mut record = [0; 2 + size_of::<Token>()];
    record[..2].copy_from_slice(&port.to_le_bytes());
    record[2..].copy_from_slice(&token);
    fs::write(path, protect_record(&mut record, true)?)?;
    Ok(token)
}

/// Compare a presented credential without leaking its matching prefix.
#[must_use]
pub fn token_matches(expected: &Token, received: &Token) -> bool {
    bool::from(expected.ct_eq(received))
}

// #######################################################
// authenticate before exchanging commands
// #######################################################

/// Read and authenticate to an endpoint belonging to the current user.
///
/// # Errors
/// Returns an error if the endpoint cannot be read, decrypted, or reached,
/// or if the daemon rejects the credential.
pub fn connect(path: &Path) -> io::Result<TcpStream> {
    let mut encrypted = fs::read(path)?;
    let record = protect_record(&mut encrypted, false)?;
    let [lo, hi, token @ ..] = record.as_slice() else {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    };
    if token.len() != size_of::<Token>() {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    let port = u16::from_le_bytes([*lo, *hi]);
    let mut stream = TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), HANDSHAKE_TIMEOUT)?;
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
    stream.write_all(token)?;
    let mut reply = [0];
    stream.read_exact(&mut reply)?;
    if reply != [ACCEPTED] {
        return Err(io::Error::from(io::ErrorKind::PermissionDenied));
    }
    stream.set_read_timeout(None)?;
    stream.set_write_timeout(None)?;
    Ok(stream)
}

fn protect_record(bytes: &mut [u8], encrypt: bool) -> io::Result<Vec<u8>> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(bytes.len()).map_err(io::Error::other)?,
        pbData: bytes.as_mut_ptr(),
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    // SAFETY: input points to the live slice. DPAPI owns the output allocation
    // until we finish copying its bytes and release it with LocalFree.
    unsafe {
        let ok = if encrypt {
            CryptProtectData(
                &input,
                null(),
                null(),
                null(),
                null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptUnprotectData(
                &input,
                std::ptr::null_mut(),
                null(),
                null(),
                null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let result = if output.cbData == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec()
        };
        LocalFree(output.pbData.cast());
        Ok(result)
    }
}
