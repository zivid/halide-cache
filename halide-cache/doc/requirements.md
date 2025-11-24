# Halide Compiler Cache

SE Playtime h2 2026

The goal of this project is to cache object files and other output from the Vision engine code generator, in order to
speed up iterative development on the SDK and reduce CI load.

There are two "challenges" to be tackled in this project:

## 1. Track dependencies for generated files

Since we are not aiming to create a "general purpose" Halide cache, but one that is for internal use at Zivid, we can
make some assumptions about the dependencies of the generated kernels that simplify this part:

* If any files within `sdk/vision-engine/` folder have changed, then cache should be invalidated.
* If the Halide conan package version (including revision) changed, then cache should be invalidated.

If time permits, we could look into analysis the dependency graph of each kernel to avoid invalidating the whole cache
if only one file changes in the Vision engine, but even without this the cache will be useful for many developers and
many of the builds that run in the CI.

### 1.1 Situation in the repository

There is already in the repository [dependency_finder.py](/sdk/vision-engine/scripts/dependency_finder.py) that does
a part of what we want. We could try to rewrite it in Rust to have better performance or build on top of it by using
for example Pyo3 to see if we can leverage the idea to further along the caching.

## 2. Store generated files in a cache

When we have generated a halide kernels, we need to capture all the output files and store them in a cache on the file
system. The cache has the following minimal requirements:

* Reliability
  * The cache must be resistant to corruption.
  * The cache must be resistant to unexpected termination signal or getting killed.
  * The cache must handle concurrent processes.
* Disk space management
  * The cache must be able to limit its disk space usage.
* Good hit rate
  * The cache must be able to choose which kernels to keep in the cache and which to throw out, in order to achieve a
    useful hit rate.
