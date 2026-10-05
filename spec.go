// Package fibregatewayclient embeds the gateway API spec and the commitment test vectors.
package fibregatewayclient

import _ "embed"

// OpenAPI is docs/openapi.yaml.
//
//go:embed docs/openapi.yaml
var OpenAPI []byte

// CommitmentVectors is clients/testdata/commitment_vectors.json.
//
//go:embed clients/testdata/commitment_vectors.json
var CommitmentVectors []byte
