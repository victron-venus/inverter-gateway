#![no_main]
libfuzzer_sys::fuzz_target!(|data: &[u8]| inverter_gateway_fuzz::command_payload(data));
