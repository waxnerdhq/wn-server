package wncloud

import (
	"crypto/rand"
	"crypto/sha256"
	"encoding/base64"
)

// NewVerifier returns a PKCE code verifier: 32 random bytes, base64url
// without padding, 43 characters.
func NewVerifier() (string, error) { return randomURLSafe(32) }

// Challenge returns the S256 PKCE challenge of a verifier:
// base64url(sha256(verifier)) without padding.
func Challenge(verifier string) string {
	sum := sha256.Sum256([]byte(verifier))
	return base64.RawURLEncoding.EncodeToString(sum[:])
}

// NewState returns a sign-in state: 16 random bytes, base64url without
// padding, 22 characters.
func NewState() (string, error) { return randomURLSafe(16) }

func randomURLSafe(n int) (string, error) {
	b := make([]byte, n)
	if _, err := rand.Read(b); err != nil {
		return "", err
	}
	return base64.RawURLEncoding.EncodeToString(b), nil
}
