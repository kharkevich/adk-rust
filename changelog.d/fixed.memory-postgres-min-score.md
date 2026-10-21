- **PostgreSQL vector search honours `min_score`** (`adk-memory`): with an embedding
  provider, any `min_score` dropped every hit, because the cosine distance pgvector returns
  as `float8` was read as `f32` and the failed read scored each entry 0. Hits at least that
  similar are now kept, on `vector` and `halfvec` indexes. The harness's
  `MemoryServiceIndex` conformance suite on PostgreSQL, which passes `min_score`, passes
  again.
