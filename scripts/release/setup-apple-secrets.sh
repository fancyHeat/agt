#!/bin/bash
# Stores AGT's macOS signing certificate as secrets of the GitHub `release` environment, which
# the package-macos job in .github/workflows/release.yml uses to sign and notarize. Run it in Terminal on the Mac whose login
# keychain holds the Developer ID Application identity:
#
#   bash setup-apple-secrets.sh                 # export the identity; set APPLE_CERT_P12_BASE64
#                                               # and APPLE_CERT_PASSWORD
#   bash setup-apple-secrets.sh --with-apple-id # also (re)set APPLE_SIGN_IDENTITY, APPLE_TEAM_ID,
#                                               # APPLE_ID and APPLE_APP_PASSWORD (prompts)
#   bash setup-apple-secrets.sh --dry-run       # only check the prerequisites; changes nothing
#
# Nothing secret is echoed, passed to `gh` on its command line, or written outside a private temp
# directory that is deleted on exit. macOS asks once to allow the private key export (enter your
# login password, click "Allow"). The .p12 password is random and only ever stored in GitHub.
set -euo pipefail

REPO=fancyHeat/agt
ENVIRONMENT=release
TEAM_ID=TAYJMC7G8C
IDENTITY="Developer ID Application: NOTICS Security LLC ($TEAM_ID)"
LOGIN_KEYCHAIN="$HOME/Library/Keychains/login.keychain-db"

DRY_RUN=false
WITH_APPLE_ID=false
for arg in "$@"; do
  case "$arg" in
    --dry-run) DRY_RUN=true ;;
    --with-apple-id) WITH_APPLE_ID=true ;;
    *) echo "usage: $0 [--dry-run] [--with-apple-id]" >&2; exit 2 ;;
  esac
done

die() { echo "error: $*" >&2; exit 1; }
step() { echo "==> $*"; }

# --- Prerequisites -----------------------------------------------------------------------------
[ "$(uname -s)" = Darwin ] || die "run this on the Mac that holds the signing identity"
for tool in gh security xcrun base64 openssl; do
  command -v "$tool" >/dev/null || die "'$tool' not found"
done
xcrun --find notarytool >/dev/null 2>&1 || die "notarytool not found (install Xcode or its command line tools)"

step "Checking GitHub CLI login and access to the '$ENVIRONMENT' environment of $REPO"
gh auth status >/dev/null 2>&1 || die "gh is not logged in (run: gh auth login)"
gh api "repos/$REPO/environments/$ENVIRONMENT" --jq .name >/dev/null 2>&1 \
  || die "can't read the '$ENVIRONMENT' environment of $REPO (wrong account or missing admin rights?)"

step "Looking for \"$IDENTITY\""
# SHA-1 hashes of the valid (unexpired, trusted) code-signing identities with exactly that name.
VALID_HASHES="$(security find-identity -v -p codesigning "$LOGIN_KEYCHAIN" \
  | awk -v id="\"$IDENTITY\"" '/^ *[0-9]+\)/ && substr($0, index($0, "\"")) == id {print $2}' | sort -u)"
[ -n "$VALID_HASHES" ] || die "no valid \"$IDENTITY\" in $LOGIN_KEYCHAIN (missing, expired or revoked?)"
if [ "$(printf '%s\n' "$VALID_HASHES" | wc -l | tr -d ' ')" != 1 ]; then
  echo "error: more than one valid \"$IDENTITY\":" >&2
  printf '%s\n' "$VALID_HASHES" | while read -r h; do
    echo "    $h, $(security find-certificate -a -c "$IDENTITY" -Z -p "$LOGIN_KEYCHAIN" \
      | awk -v h="$h" '/^SHA-1 hash:/ {m = ($3 == h)} m' | sed -n '/BEGIN CERT/,/END CERT/p' \
      | openssl x509 -noout -enddate)" >&2
  done
  die "keep only the one to use (Keychain Access -> login -> My Certificates; delete the other) and rerun"
fi
IDENTITY_HASH="$VALID_HASHES"
# `security export` can't select one identity: it exports every identity in the keychain (any
# policy, valid or not), so it's only usable when this is the only one.
IDENTITY_COUNT="$(security find-identity "$LOGIN_KEYCHAIN" \
  | awk '/Matching identities/{m=1;next} /identities found/{m=0} m && /^ *[0-9]+\)/ {print $2}' | sort -u | wc -l | tr -d ' ')"
echo "    found $IDENTITY_HASH; the login keychain holds $IDENTITY_COUNT identit$([ "$IDENTITY_COUNT" = 1 ] && echo y || echo ies) in total"

VALID_ANY="$(security find-identity -v "$LOGIN_KEYCHAIN")"
if [[ "$VALID_ANY" == *'"Developer ID Installer:'* ]]; then
  echo "    note: a Developer ID Installer identity exists, but this script doesn't set"
  echo "    APPLE_INSTALLER_IDENTITY; releases ship the macOS .zip only."
else
  echo "    no Developer ID Installer identity: APPLE_INSTALLER_IDENTITY is skipped (no .pkg, .zip only)"
fi

EXISTING="$(gh secret list --env "$ENVIRONMENT" --repo "$REPO" --json name --jq '[.[].name | select(startswith("APPLE_"))] | join(" ")')"
echo "    existing APPLE_* secrets in '$ENVIRONMENT': ${EXISTING:-none}"
if [ "$WITH_APPLE_ID" = false ]; then
  MISSING=""
  for name in APPLE_SIGN_IDENTITY APPLE_TEAM_ID APPLE_ID APPLE_APP_PASSWORD; do
    case " $EXISTING " in *" $name "*) ;; *) MISSING="$MISSING $name" ;; esac
  done
  [ -z "$MISSING" ] || echo "    warning: missing$MISSING; run again with --with-apple-id to set them"
fi

if [ "$DRY_RUN" = true ]; then
  step "Dry run: prerequisites OK; nothing was exported or changed"
  exit 0
fi

# --- Export the identity -----------------------------------------------------------------------
umask 077
WORK="$(mktemp -d "${TMPDIR:-/tmp}/agt-signing.XXXXXX")"
CHECK_KEYCHAIN="$WORK/check.keychain-db"
# The user keychain search list, one path per element (paths may contain spaces).
ORIGINAL_SEARCH_LIST=()
while IFS= read -r kc; do
  kc="${kc#*\"}"; ORIGINAL_SEARCH_LIST+=("${kc%\"*}")
done < <(security list-keychains -d user)
restore_search_list() {
  security list-keychains -d user -s ${ORIGINAL_SEARCH_LIST[@]+"${ORIGINAL_SEARCH_LIST[@]}"}
}
cleanup() {
  security delete-keychain "$CHECK_KEYCHAIN" >/dev/null 2>&1 || true
  restore_search_list >/dev/null 2>&1 || true
  # APFS on an SSD can't be securely overwritten in place; overwrite anyway, then delete.
  if [ -f "$WORK/agt.p12" ]; then
    dd if=/dev/urandom of="$WORK/agt.p12" bs=1k count=16 conv=notrunc >/dev/null 2>&1 || true
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

P12="$WORK/agt.p12"
P12_PASSWORD="$(openssl rand -base64 36 | tr -d '\n/+=')"

if [ "$IDENTITY_COUNT" = 1 ]; then
  step "Exporting the identity (certificate + private key) to a temporary .p12"
  echo "    macOS will ask to allow exporting the private key: enter your login password and click Allow."
  # The passphrase is visible to `ps` for the moment `security` runs; it only protects a file that
  # is deleted when this script exits.
  security export -k "$LOGIN_KEYCHAIN" -t identities -f pkcs12 -P "$P12_PASSWORD" -o "$P12" >/dev/null \
    || die "export failed (did you click Deny?)"
else
  step "Your login keychain holds more than one identity, so export it by hand:"
  echo "    1. Open Keychain Access -> login -> My Certificates."
  echo "    2. Right-click \"$IDENTITY\" with SHA-1 fingerprint $IDENTITY_HASH"
  echo "       (only that one) -> Export..."
  echo "    3. Format: Personal Information Exchange (.p12). Save it as:"
  echo "         $P12"
  echo "       (in the save dialog press Cmd+Shift+G and paste the folder: $WORK)"
  echo "    4. Choose any strong password; you'll type it here next."
  read -r -p "    Press Return when $P12 exists... " _
  [ -f "$P12" ] || die "$P12 not found"
  read -r -s -p "    Password you chose for the .p12: " P12_PASSWORD; echo
  [ -n "$P12_PASSWORD" ] || die "empty password"
fi

step "Checking that the .p12 contains exactly that identity"
# Import into a throwaway keychain, as CI does, and list what came out.
CHECK_PASSWORD="$(openssl rand -hex 16)"
security create-keychain -p "$CHECK_PASSWORD" "$CHECK_KEYCHAIN"
restore_search_list # create-keychain may add the new keychain to the search list
security unlock-keychain -p "$CHECK_PASSWORD" "$CHECK_KEYCHAIN"
security import "$P12" -k "$CHECK_KEYCHAIN" -f pkcs12 -P "$P12_PASSWORD" >/dev/null \
  || die "the .p12 couldn't be imported (wrong password?)"
# Every identity (valid or not) that the .p12 brought in, space-separated.
P12_IDENTITIES="$(security find-identity "$CHECK_KEYCHAIN" \
  | awk '/Matching identities/{m=1;next} /identities found/{m=0} m && /^ *[0-9]+\)/ {printf "%s%s", s, $2; s=" "}')"
[ "$P12_IDENTITIES" = "$IDENTITY_HASH" ] \
  || die "the .p12 must contain exactly $IDENTITY_HASH; it contains: ${P12_IDENTITIES:-nothing}"
security delete-keychain "$CHECK_KEYCHAIN"
echo "    OK"

# --- Apple ID for notarization (only with --with-apple-id) -------------------------------------
if [ "$WITH_APPLE_ID" = true ]; then
  step "Notarization credentials"
  echo "    Use an app-specific password, not your Apple ID password. Create one at"
  echo "    https://account.apple.com -> Sign-In and Security -> App-Specific Passwords -> +"
  echo "    (name it e.g. \"AGT notarization\")."
  read -r -p "    Apple ID email (a member of team $TEAM_ID): " APPLE_ID
  [ -n "$APPLE_ID" ] || die "empty Apple ID"
  read -r -s -p "    App-specific password (hidden): " APPLE_APP_PASSWORD; echo
  [ -n "$APPLE_APP_PASSWORD" ] || die "empty password"

  step "Checking those credentials with Apple (notarytool history)"
  # The password is visible to `ps` while notarytool runs (it can't read it from stdin).
  xcrun notarytool history --apple-id "$APPLE_ID" --team-id "$TEAM_ID" --password "$APPLE_APP_PASSWORD" \
    --output-format json >/dev/null 2>&1 \
    || die "Apple rejected the Apple ID / app-specific password for team $TEAM_ID (nothing was set)"
  echo "    OK"
fi

# --- Store the secrets -------------------------------------------------------------------------
step "Setting secrets of the '$ENVIRONMENT' environment of $REPO"
# Values go to gh on stdin (printf is a shell builtin, so they never appear in `ps`).
set_secret() { gh secret set "$1" --env "$ENVIRONMENT" --repo "$REPO" >/dev/null && echo "    set $1"; }
base64 -i "$P12" | tr -d '\n' | set_secret APPLE_CERT_P12_BASE64
printf '%s' "$P12_PASSWORD" | set_secret APPLE_CERT_PASSWORD
if [ "$WITH_APPLE_ID" = true ]; then
  printf '%s' "$IDENTITY" | set_secret APPLE_SIGN_IDENTITY
  printf '%s' "$TEAM_ID" | set_secret APPLE_TEAM_ID
  printf '%s' "$APPLE_ID" | set_secret APPLE_ID
  printf '%s' "$APPLE_APP_PASSWORD" | set_secret APPLE_APP_PASSWORD
  unset APPLE_APP_PASSWORD
fi
unset P12_PASSWORD

step "Done. Secrets now in the '$ENVIRONMENT' environment (names only):"
gh secret list --env "$ENVIRONMENT" --repo "$REPO" --json name --jq '.[].name' | sed 's/^/    /'
