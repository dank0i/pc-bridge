#!/bin/bash
# Quality check script for pc-bridge
# Run all quality checks before committing/pushing

set -e

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

echo "----------------------------------------------------------------------"
echo "                    pc-bridge Quality Checks"
echo "----------------------------------------------------------------------"
echo ""

FAILED=0

# Formatting
echo -e "${YELLOW}[1/7]${NC} Checking formatting..."
if cargo fmt --check; then
    echo -e "${GREEN}OK${NC} Formatting OK"
else
    echo -e "${RED}FAIL${NC} Formatting issues found. Run: cargo fmt"
    FAILED=1
fi
echo ""

# Clippy
echo -e "${YELLOW}[2/7]${NC} Running Clippy..."
if cargo clippy -- -D warnings 2>&1 | grep -q "warning\|error"; then
    echo -e "${RED}FAIL${NC} Clippy found issues"
    FAILED=1
else
    echo -e "${GREEN}OK${NC} Clippy OK"
fi
echo ""

# Windows cross-compile check
echo -e "${YELLOW}[3/7]${NC} Checking Windows build..."
if rustup target list --installed 2>/dev/null | grep -q "x86_64-pc-windows-gnu"; then
    WIN_OUTPUT=$(cargo check --target x86_64-pc-windows-gnu 2>&1)
    if echo "$WIN_OUTPUT" | grep -q "^error"; then
        echo -e "${RED}FAIL${NC} Windows build failed"
        echo "$WIN_OUTPUT" | grep "^error"
        FAILED=1
    else
        echo -e "${GREEN}OK${NC} Windows build OK"
    fi
else
    echo -e "${YELLOW}!!${NC} Windows target not installed. Run: rustup target add x86_64-pc-windows-gnu"
fi
echo ""

# Tests
echo -e "${YELLOW}[4/7]${NC} Running tests..."
if cargo test 2>&1 | tail -5 | grep -q "FAILED"; then
    echo -e "${RED}FAIL${NC} Tests failed"
    FAILED=1
else
    echo -e "${GREEN}OK${NC} Tests OK"
fi
echo ""

# Audit via cargo-deny (respects deny.toml ignore list)
echo -e "${YELLOW}[5/7]${NC} Checking for vulnerabilities..."
if command -v cargo-deny &> /dev/null; then
    if cargo deny check advisories 2>&1 | grep -q "error\["; then
        echo -e "${RED}FAIL${NC} Vulnerabilities found"
        FAILED=1
    else
        echo -e "${GREEN}OK${NC} No vulnerabilities"
    fi
else
    echo -e "${YELLOW}!!${NC} cargo-deny not installed. Run: cargo install cargo-deny"
fi
echo ""

# Deny - licenses and bans
echo -e "${YELLOW}[6/7]${NC} Checking dependency policy..."
if command -v cargo-deny &> /dev/null; then
    if cargo deny check licenses bans 2>&1 | grep -q "error\["; then
        echo -e "${RED}FAIL${NC} Dependency policy violations"
        FAILED=1
    else
        echo -e "${GREEN}OK${NC} Dependencies OK"
    fi
else
    echo -e "${YELLOW}!!${NC} cargo-deny not installed. Run: cargo install cargo-deny"
fi
echo ""

# Secrets check (if installed)
echo -e "${YELLOW}[7/7]${NC} Checking for secrets..."
if command -v gitleaks &> /dev/null; then
    LEAKS_OUTPUT=$(gitleaks detect 2>&1)
    if echo "$LEAKS_OUTPUT" | grep -q "no leaks found"; then
        echo -e "${GREEN}OK${NC} No secrets found"
    else
        echo -e "${RED}FAIL${NC} Secrets detected in code"
        FAILED=1
    fi
else
    echo -e "${YELLOW}!!${NC} gitleaks not installed. Run: brew install gitleaks"
fi
echo ""

echo "----------------------------------------------------------------------"
if [ $FAILED -eq 0 ]; then
    echo -e "${GREEN}All checks passed!${NC}"
    exit 0
else
    echo -e "${RED}Some checks failed. Please fix before committing.${NC}"
    exit 1
fi
