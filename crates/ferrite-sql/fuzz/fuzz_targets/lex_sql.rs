#![no_main]

use ferrite_sql::lexer::Lexer;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(sql) = std::str::from_utf8(data) {
        let _ = Lexer::new(sql).tokenize();
    }
});
