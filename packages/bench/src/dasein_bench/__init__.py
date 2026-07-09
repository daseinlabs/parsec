"""dasein-bench — cc-bench harness, grader, and arms.

Sits at the top of the dependency graph: bench -> proxy -> engine. It drives
the `dasein` binary as a black box and is never imported by product code.
"""
