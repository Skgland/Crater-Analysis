cargo run --release -- $(Get-ChildItem -Path results -Directory | Select-Object -expand Name)