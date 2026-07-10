"""Concrete arms. Importing this package registers every arm.

The registry (and the built-in `baseline` control) lives in dasein_bench.arm;
one module per arm here, self-registering via @register on import.
"""

from dasein_bench.arms import dasein  # noqa: F401  (side effect: registers "dasein")
