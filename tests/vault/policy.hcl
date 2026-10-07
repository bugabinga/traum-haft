# Policy for the traum-haft gateway (same as production).
path "kv/data/traum-haft/*" {
  capabilities = ["create", "read", "update", "delete"]
}
