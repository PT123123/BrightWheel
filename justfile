# BrightWheel (Rust / Slint) - command entry points
# Run `just` with no arguments to list available commands.

set shell := ["powershell.exe", "-NoProfile", "-ExecutionPolicy", "Bypass", "-c"]

# List available commands
default:
    @just --list

# Incremental release build -> target\release\BrightWheel.exe
build:
    @cargo build --release

# Build and launch the tray app
run: build
    @& .\target\release\BrightWheel.exe

# Bump patch version, rebuild, deploy to C:\workshop\BrightWheel-<version>
deploy-workshop:
    @& .\tools\deploy_workshop.ps1
