# Strategy template

A pristine copy of `plugins/strategy/src/`: the moving-average crossover template and its tests.
It is not a workspace member and is never compiled from here.

To start a new strategy from the template, from the `rt` directory:

```bash
rm -rf plugins/strategy/src
cp -r plugins/strategy-template/src plugins/strategy/src
```

Edit only `plugins/strategy/src/`. The slot's `Cargo.toml` is frozen.
