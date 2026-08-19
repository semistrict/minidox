# Fork memory and filesystem atomically

Forking pauses the Source VM and establishes one Fork Point covering both guest memory and filesystem state before the Source VM and Child VM continue independently. The filesystem side seals the current in-memory Filesystem Generation, including guest-visible DAX and buffered writes, without waiting for durable storage I/O. The pause must remain below 10 ms at p99 and must not scale with guest RAM or filesystem size; work during the pause is limited to draining in-flight virtio operations and publishing snapshot metadata. This prevents a child from starting with memory that refers to filesystem state from a different instant without copying pages in the pause path.

Successful fork creation survives a filesystem failure through crash-consistent generation metadata. Host power-loss durability is required only for a Durable Fork; ordinary forks may flush page contents asynchronously.
