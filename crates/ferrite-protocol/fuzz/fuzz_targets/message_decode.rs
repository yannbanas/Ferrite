//! Message decoding must be total: any byte string is either a valid
//! message or a clean error, never a panic.
#![no_main]

use libfuzzer_sys::fuzz_target;

use ferrite_protocol::message::{Frontend, StartupRequest};

fuzz_target!(|data: &[u8]| {
    let Some((tag, body)) = data.split_first() else {
        return;
    };
    let _ = Frontend::decode(*tag, body);
    let _ = StartupRequest::decode(body);
    // Also with the whole input as the body, so the startup decoder sees
    // inputs whose first byte was not consumed as a tag.
    let _ = StartupRequest::decode(data);
});
