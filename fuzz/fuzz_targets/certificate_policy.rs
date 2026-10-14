#![no_main]
libfuzzer_sys::fuzz_target!(|data: &[u8]| inverter_gateway_fuzz::certificate_policy(data));
