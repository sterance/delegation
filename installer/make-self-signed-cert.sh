#!/bin/sh
# Makes the self-signed code signing certificate for windows.yml's
# "self-signed" signing mode (installer/SIGNING.md), on Linux or macOS.
# Run it once, then add the secrets it prints. It never writes inside the
# repository.
#
#   installer/make-self-signed-cert.sh [output dir]    (default ~/.delegation-signing)
#
# Writes to the output dir:
#   delegation-code-signing.pfx      certificate and private key, password-protected
#   delegation-code-signing.password the PFX password
#   delegation-code-signing.cer      the public certificate (also published with each release)
set -eu

out="${1:-$HOME/.delegation-signing}"
name="Chris Smith (Delegation, self-signed)"
days=1095 # 3 years; timestamped signatures stay valid after that

umask 077
mkdir -p "$out"
pfx="$out/delegation-code-signing.pfx"
if [ -e "$pfx" ]; then
    echo "$pfx already exists; delete it first to make a new certificate." >&2
    exit 1
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# Code signing only (extended key usage), like New-SelfSignedCertificate
# -Type CodeSigningCert makes on Windows.
cat >"$tmp/req.cnf" <<EOF
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = $name
[ext]
keyUsage = critical, digitalSignature
extendedKeyUsage = codeSigning
subjectKeyIdentifier = hash
EOF

openssl req -x509 -newkey rsa:3072 -sha256 -days "$days" -nodes \
    -config "$tmp/req.cnf" -keyout "$tmp/key.pem" -out "$tmp/cert.pem" 2>/dev/null

password=$(openssl rand -base64 32)
printf '%s' "$password" >"$out/delegation-code-signing.password"

# AES and SHA-256 throughout, and the key in the CSP Windows's signing API
# (which Set-AuthenticodeSignature uses) can read.
openssl pkcs12 -export -inkey "$tmp/key.pem" -in "$tmp/cert.pem" -out "$pfx" \
    -name "$name" -passout "pass:$password" \
    -keypbe AES-256-CBC -certpbe AES-256-CBC -macalg sha256 \
    -CSP "Microsoft Enhanced RSA and AES Cryptographic Provider"

openssl x509 -in "$tmp/cert.pem" -outform der -out "$out/delegation-code-signing.cer"

thumbprint=$(openssl x509 -in "$tmp/cert.pem" -noout -fingerprint -sha1 | sed 's/.*=//; s/://g')
echo "Made $pfx"
echo "  subject     CN=$name"
echo "  thumbprint  $thumbprint"
echo "  expires     $(openssl x509 -in "$tmp/cert.pem" -noout -enddate | sed 's/.*=//')"
echo
cat <<EOF
Add it to GitHub (from the repository directory, with the gh CLI):
  base64 < "$pfx" | tr -d '\n' | gh secret set SELF_SIGN_PFX_BASE64
  gh secret set SELF_SIGN_PFX_PASSWORD < "$out/delegation-code-signing.password"
  gh variable set WINDOWS_SIGNING --body self-signed

Keep $out private and backed up: anyone with the PFX and its password can sign as you.
EOF
