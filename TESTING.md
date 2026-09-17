# Test Instructions

Run the full test suite locally:

```powershell
$Env:RUSTUP_HOME='D:\rust\rustup'
$Env:CARGO_HOME='D:\rust\cargo'
$Env:CARGO_TARGET_DIR='D:\rust\target'
$Env:PATH='D:\rust\w64devkit\bin;D:\rust\cargo\bin;' + $Env:PATH

cargo test -p parsec-engine
```

All 55 engine unit tests and the parity tests should pass.
