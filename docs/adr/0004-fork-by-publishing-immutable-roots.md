# Fork by publishing immutable roots

Each Filesystem Generation is identified by an immutable RedoxFS tree root. Forking creates parent and child branches by publishing root references without copying pages or walking the filesystem, keeping pause work independent of filesystem size. Unchanged blocks remain shared, post-fork writes create branch-local blocks, and unreachable generations are reclaimed outside the pause path.
