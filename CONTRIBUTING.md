# Contributing to Orbit

We love your input! We want to make contributing to Orbit as easy and transparent as possible, whether it's:

- Reporting a bug
- Discussing the current state of the code
- Submitting a fix
- Proposing new features
- Becoming a maintainer

## Development Process

We use GitHub to host code, to track issues and feature requests, as well as accept pull requests.

1. Fork the repo and create your branch from `main`.
2. If you've added code that should be tested, add tests.
3. If you've changed APIs, update the documentation.
4. Ensure the test suite passes.
5. Make sure your code lints.
6. Issue that pull request!


## Code Style and Standards (not enforced yet)

We use rustfmt and clippy to maintain code quality. Before submitting a PR:

1. Format your code:
   ```bash
   cargo fmt
   ```

2. Run clippy:
   ```bash
   cargo clippy -- -D warnings
   ```

3. Run tests:
   ```bash
   cargo test
   ```

### Coding Conventions

- Use meaningful variable names
- Write comments for complex logic
- Include docstrings for public APIs
- Follow the Rust API guidelines
- Keep functions focused and small
- Use type safety wherever possible

## Pull Request Process

1. Update the README.md with details of changes to the interface, if applicable.
2. Update any relevant documentation in the `docs` directory.
3. Update the examples if you've added new features.
4. The PR will be merged once you have the sign-off of at least one maintainer.

## Any contributions you make will be under the Apache License 2.0

In short, when you submit code changes, your submissions are understood to be under the same [Apache License 2.0](LICENSE) that covers the project. The Apache License 2.0 includes explicit patent rights and requires stating significant changes made to files. Feel free to contact the maintainers if you have any questions about this.

## Report bugs using GitHub's [issue tracker](https://github.com/airpipeio/orbit/issues)

We use GitHub issues to track public bugs. Report a bug by [opening a new issue](https://github.com/airpipeio/orbit/issues/new/choose).

### Write bug reports with detail, background, and sample code

**Great Bug Reports** tend to have:

- A quick summary and/or background
- Steps to reproduce
  - Be specific!
  - Give sample code if you can.
- What you expected would happen
- What actually happens
- Notes (possibly including why you think this might be happening, or stuff you tried that didn't work)

## License

By contributing, you agree that your contributions will be licensed under its Apache License 2.0.

