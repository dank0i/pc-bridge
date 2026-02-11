#!/usr/bin/env bash
# Build script for pc-bridge
# Usage: ./scripts/build.sh [--release] [--check-only] [--version X.Y.Z] [--tag]

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
cd "$PROJECT_DIR"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

# Defaults
RELEASE=false
CHECK_ONLY=false
NEW_VERSION=""
CREATE_TAG=false

# Parse args
while [[ $# -gt 0 ]]; do
    case $1 in
        --release|-r)
            RELEASE=true
            shift
            ;;
        --check-only|-c)
            CHECK_ONLY=true
            shift
            ;;
        --version|-v)
            NEW_VERSION="$2"
            shift 2
            ;;
        --tag|-t)
            CREATE_TAG=true
            shift
            ;;
        --help|-h)
            echo "Usage: $0 [OPTIONS]"
            echo ""
            echo "Options:"
            echo "  -r, --release        Build in release mode (optimized)"
            echo "  -c, --check-only     Run quality checks without building"
            echo "  -v, --version X.Y.Z  Bump version in Cargo.toml (e.g., 2.5.0)"
            echo "  -t, --tag            Commit, tag, and push (requires --version, skips local build)"
            echo "  -h, --help           Show this help"
            echo ""
            echo "Examples:"
            echo "  $0 --release                        # Release build locally"
            echo "  $0 --version 2.5.0 --release        # Bump version and build locally"
            echo "  $0 --version 2.5.0 --tag            # Full release (GitHub builds binary)"
            echo "  $0 --check-only                     # Just run checks"
            echo ""
            echo "The --tag option will:"
            echo "  1. Run quality checks (fmt, clippy, tests, audit, deny)"
            echo "  2. Commit all staged changes with message 'Release vX.Y.Z'"
            echo "  3. Create annotated git tag vX.Y.Z"
            echo "  4. Push commits and tags to origin"
            echo "  5. GitHub Actions builds Windows binary and attaches to release"
            exit 0
            ;;
        *)
            echo -e "${RED}Unknown option: $1${NC}"
            exit 1
            ;;
    esac
done

# Validate --tag requires --version
if $CREATE_TAG && [[ -z "$NEW_VERSION" ]]; then
    echo -e "${RED}Error: --tag requires --version${NC}"
    echo -e "Usage: $0 --version X.Y.Z --release --tag"
    exit 1
fi

echo -e "${BLUE}--------------------------------------------------------------------------${NC}"
echo -e "${BLUE}                         pc-bridge build                                ${NC}"
echo -e "${BLUE}--------------------------------------------------------------------------${NC}"

# Handle version bump
if [[ -n "$NEW_VERSION" ]]; then
    # Validate semver format
    if ! [[ "$NEW_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
        echo -e "${RED}Invalid version format: $NEW_VERSION${NC}"
        echo -e "Expected: X.Y.Z (e.g., 2.5.0)"
        exit 1
    fi
    
    OLD_VERSION=$(grep '^version = ' Cargo.toml | head -1 | sed 's/version = "\(.*\)"/\1/')
    echo -e "Bumping version: ${YELLOW}${OLD_VERSION}${NC} → ${GREEN}${NEW_VERSION}${NC}"
    
    # Update Cargo.toml
    sed -i.bak "s/^version = \"$OLD_VERSION\"/version = \"$NEW_VERSION\"/" Cargo.toml
    rm -f Cargo.toml.bak
    
    # Update Cargo.lock
    cargo update -p pc-bridge 2>/dev/null || true
    
    echo -e "  ${GREEN}OK${NC} Version updated in Cargo.toml"
    echo ""
fi

# Get version from Cargo.toml
VERSION=$(grep '^version = ' Cargo.toml | head -1 | sed 's/version = "\(.*\)"/\1/')
echo -e "Version: ${GREEN}${VERSION}${NC}"
echo ""

# Step 1: Format check
echo -e "${YELLOW}[1/5]${NC} Checking formatting..."
if cargo fmt --check 2>/dev/null; then
    echo -e "  ${GREEN}OK${NC} Formatting OK"
else
    echo -e "  ${RED}FAIL${NC} Formatting issues found"
    echo -e "  Run: ${YELLOW}cargo fmt${NC} to fix"
    exit 1
fi

# Step 2: Clippy
echo -e "${YELLOW}[2/5]${NC} Running Clippy..."
if cargo clippy --all-targets -- -D warnings 2>/dev/null; then
    echo -e "  ${GREEN}OK${NC} Clippy OK"
else
    echo -e "  ${RED}FAIL${NC} Clippy found issues"
    exit 1
fi

# Step 3: Tests
echo -e "${YELLOW}[3/5]${NC} Running tests..."
if cargo test --quiet 2>/dev/null; then
    TEST_COUNT=$(cargo test 2>&1 | grep -E "^test result:" | grep -oE "[0-9]+ passed" | head -1)
    echo -e "  ${GREEN}OK${NC} Tests OK (${TEST_COUNT})"
else
    echo -e "  ${RED}FAIL${NC} Tests failed"
    exit 1
fi

# Step 4: Security audit
echo -e "${YELLOW}[4/5]${NC} Security audit..."
if command -v cargo-audit &> /dev/null; then
    if cargo audit --quiet 2>/dev/null; then
        echo -e "  ${GREEN}OK${NC} No vulnerabilities"
    else
        echo -e "  ${YELLOW}!!${NC} Warnings found (check with cargo audit)"
    fi
else
    echo -e "  ${YELLOW}!!${NC} cargo-audit not installed"
fi

# Step 5: Dependency policy
echo -e "${YELLOW}[5/5]${NC} Dependency policy..."
if command -v cargo-deny &> /dev/null; then
    if cargo deny check 2>/dev/null | tail -1 | grep -q "ok"; then
        echo -e "  ${GREEN}OK${NC} Dependencies OK"
    else
        echo -e "  ${RED}FAIL${NC} Dependency policy failed"
        exit 1
    fi
else
    echo -e "  ${YELLOW}!!${NC} cargo-deny not installed"
fi

echo ""

if $CHECK_ONLY; then
    echo -e "${GREEN}--------------------------------------------------------------------------${NC}"
    echo -e "${GREEN}All checks passed!${NC}"
    echo -e "${GREEN}--------------------------------------------------------------------------${NC}"
    exit 0
fi

# Handle git tag and push (skips local build - GitHub Actions builds Windows binary)
if $CREATE_TAG; then
    echo -e "${BLUE}Creating release...${NC}"
    echo -e "  ${YELLOW}ℹ${NC} Skipping local build (GitHub Actions will build Windows binary)"
    
    TAG="v${VERSION}"
    
    # Stage all changes
    git add -A
    
    # Check if there are changes to commit
    if git diff --cached --quiet; then
        echo -e "  ${YELLOW}!!${NC} No changes to commit"
    else
        git commit -m "Release ${TAG}"
        echo -e "  ${GREEN}OK${NC} Committed: Release ${TAG}"
    fi
    
    # Create tag (delete if exists locally)
    if git tag -l | grep -q "^${TAG}$"; then
        echo -e "  ${YELLOW}!!${NC} Tag ${TAG} already exists locally, recreating..."
        git tag -d "${TAG}" >/dev/null
    fi
    
    git tag -a "${TAG}" -m "Release ${TAG}"
    echo -e "  ${GREEN}OK${NC} Created tag: ${TAG}"
    
    # Push
    echo -e "  Pushing to origin..."
    git push origin main --tags
    echo -e "  ${GREEN}OK${NC} Pushed to origin"
    
    echo ""
    echo -e "${GREEN}--------------------------------------------------------------------------${NC}"
    echo -e "${GREEN}Release ${TAG} complete!${NC}"
    echo -e "${GREEN}--------------------------------------------------------------------------${NC}"
    echo ""
    echo -e "${YELLOW}Next steps:${NC}"
    echo -e "  1. Go to: ${BLUE}https://github.com/dank0i/pc-bridge/releases/new?tag=${TAG}${NC}"
    echo -e "  2. Create release notes (or let GitHub auto-generate)"
    echo -e "  3. GitHub Actions will build and attach the Windows binary"
    exit 0
fi

# Build (only when not using --tag)
echo -e "${BLUE}Building...${NC}"
if $RELEASE; then
    echo -e "Mode: ${GREEN}release${NC} (optimized)"
    cargo build --release 2>&1 | grep -E "Compiling|Finished" || true
    
    BINARY="target/release/pc-bridge"
    if [[ -f "$BINARY" ]]; then
        SIZE=$(du -h "$BINARY" | cut -f1)
        echo ""
        echo -e "${GREEN}--------------------------------------------------------------------------${NC}"
        echo -e "${GREEN}Build successful!${NC}"
        echo -e "Binary: ${BLUE}${BINARY}${NC}"
        echo -e "Size:   ${BLUE}${SIZE}${NC}"
        echo -e "${GREEN}--------------------------------------------------------------------------${NC}"
        
        # Size warning
        SIZE_BYTES=$(stat -f%z "$BINARY" 2>/dev/null || stat -c%s "$BINARY" 2>/dev/null || echo 0)
        if (( SIZE_BYTES > 25000000 )); then
            echo -e "${RED}!! Warning: Binary exceeds 25MB!${NC}"
        elif (( SIZE_BYTES > 15000000 )); then
            echo -e "${YELLOW}!! Note: Binary exceeds 15MB${NC}"
        fi
    fi
else
    echo -e "Mode: ${YELLOW}debug${NC}"
    cargo build 2>&1 | grep -E "Compiling|Finished" || true
    
    BINARY="target/debug/pc-bridge"
    if [[ -f "$BINARY" ]]; then
        SIZE=$(du -h "$BINARY" | cut -f1)
        echo ""
        echo -e "${GREEN}--------------------------------------------------------------------------${NC}"
        echo -e "${GREEN}Build successful!${NC}"
        echo -e "Binary: ${BLUE}${BINARY}${NC}"
        echo -e "Size:   ${BLUE}${SIZE}${NC}"
        echo -e "${GREEN}--------------------------------------------------------------------------${NC}"
    fi
fi
