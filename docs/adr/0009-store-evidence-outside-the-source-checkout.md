# Store evidence outside the source checkout

The durable evidence root is `/home/vimkim/.local/share/cubrid-ci-data`, selected through the normal configuration precedence, rather than a relative directory inside either a CUBRID worktree or the `cubrid-ci` source checkout. Existing historical bundles move there without a `data` compatibility link inside the renamed repository, keeping multi-gigabyte evidence stable across project renames and source-worktree operations.
