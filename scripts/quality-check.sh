#!/bin/bash
# Quality check script for pc-bridge
# Run all quality checks before committing/pushing

set -e

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "                    pc-bridge Quality Checks"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo ""

FAILED=0

# Formatting
echo -e "${YELLOW}[1/6]${NC} Checking formatting..."
if cargo fmt --check; then
    echo -e "${GREEN}✓${NC} Formatting OK"
else
    echo -e "${RED}✗${NC} Formatting issues found. Run: cargo fmt"
    FAILED=1
fi
echo ""

# Clippy
echo -e "${YELLOW}[2/6]${NC} Running Clippy..."
if cargo clippy -- -D warnings 2>&1 | grep -q "warning\|error"; then
    echo -e "${RED}✗${NC} Clippy found issues"
    FAILED=1
else
    echo -e "${GREEN}✓${NC} Clippy OK"
fi
echo ""

# Tests
echo -e "${YELLOW}[3/6]${NC} Running tests..."
if cargo test 2>&1 | tail -5 | grep -q "FAILED"; then
    echo -e "${RED}✗${NC} Tests failed"
    FAILED=1
else
    echo -e "${GREEN}✓${NC} Tests OK"
fi
echo ""

# Audit (if installed)
echo -e "${YELLOW}[4/6]${NC} Checking for vulnerabilities..."
if command -v cargo-audit &> /dev/null; then
    if cargo audit 2>&1 | grep -q "Crate:"; then
        echo -e "${RED}✗${NC} Vulnerabilities found"
        FAILED=1
    else
        echo -e "${GREEN}✓${NC} No vulnerabilities"
    fi
else
    echo -e "${YELLOW}⚠${NC} cargo-audit not installed. Run: cargo install cargo-audit"
fi
echo ""

# Deny (if installed)
echo -e "${YELLOW}[5/6]${NC} Checking dependency policy..."
if command -v cargo-deny &> /dev/null; then
    if cargo deny check 2>&1 | grep -q "error\["; then
        echo -e "${RED}✗${NC} Dependency policy violations"
        FAILED=1
    else
        echo -e "${GREEN}✓${NC} Dependencies OK"
    fi
else
    echo -e "${YELLOW}⚠${NC} cargo-deny not installed. Run: cargo install cargo-deny"
fi
echo ""

# Secrets check (if installed)
echo -e "${YELLOW}[6/6]${NC} Checking for secrets..."
if command -v gitleaks &> /dev/null; then
    if gitleaks detect --no-git 2>&1 | grep -q "leaks found"; then
        echo -e "${RED}✗${NC} Secrets detected in code"
        FAILED=1
    else
        echo -e "${GREEN}✓${NC} No secrets found"
    fi
else
    echo -e "${YELLOW}⚠${NC} gitleaks not installed. Run: brew install gitleaks"
fi
echo ""

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
if [ $FAILED -eq 0 ]; then
    echo -e "${GREEN}All checks passed!${NC}"
    exit 0
else
    echo -e "${RED}Some checks failed. Please fix before committing.${NC}"
    exit 1
fi
