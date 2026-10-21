- **Workspace hooks run only under workspace trust** (`adk-harness`, spec 3.1 §9.2 I54):
  `.adk/harness.toml` now joins the instruction digest a workspace trust decision covers
  (`ProjectInstructions::instructions_digest`, listed by `audit_files` as `kind: config`),
  so a hook added or changed after the decision is not trusted until the user decides again,
  and a hook from an untrusted workspace never runs. A workspace with that file and a
  recorded trust decision is asked once more on its next use.
