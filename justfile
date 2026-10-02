default:
    @just --list

# Enter the Nix development shell.
develop:
    nix develop

# Check the Nix flake.
check:
    nix flake check

# Update flake inputs.
update:
    nix flake update

# Show flake outputs.
show:
    nix flake show

# Format Nix files.
fmt:
    nix fmt

# Build the Rust binary.
build:
    cargo build

# Run nixbump against a package file.
run FILE URL VERSION:
    cargo run -- --file {{FILE}} --url {{URL}} --version {{VERSION}}
