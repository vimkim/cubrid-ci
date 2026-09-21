# Separate evidence collection from failure analysis

`cubrid-ci` owns exact-commit discovery, collection, normalization, and validation of CI evidence, while the `cubrid-ci-analyze` skill owns causal interpretation and report writing. This keeps repeatable provider and storage mechanics in the CLI without encoding judgment-heavy root-cause conclusions as deterministic tool output.
