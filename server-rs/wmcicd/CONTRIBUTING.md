# Contributing to webMethods Microservice CI/CD Pipeline

Thank you for your interest in contributing to this project! This document provides guidelines and conventions to ensure smooth collaboration.

## Table of Contents

- [Branch Model](#branch-model)
- [Commit Message Convention](#commit-message-convention)
- [Development Workflow](#development-workflow)
- [Pull Request Process](#pull-request-process)
- [Code Review Guidelines](#code-review-guidelines)
- [Testing Requirements](#testing-requirements)

## Branch Model

We follow a **trunk-based development** model with short-lived feature branches:

### Main Branch

- `main` - The primary branch, always in a deployable state
- Protected branch requiring pull request reviews
- All changes must go through pull requests
- CI/CD pipeline runs on every commit

### Feature Branches

- **Naming convention**: `feature/<short-description>` or `<type>/<short-description>`
  - Examples: `feature/add-order-service`, `fix/config-validation`, `docs/update-readme`
- **Lifetime**: Short-lived (ideally < 2 days, max 1 week)
- **Scope**: Small, focused changes
- **Base**: Always branch from `main`
- **Merge**: Via pull request only

### Branch Lifecycle

```
main
 │
 ├─── feature/add-payment-service (create)
 │    │
 │    ├─── commit: feat(packages): add payment service
 │    ├─── commit: test(packages): add payment service tests
 │    │
 │    └─── PR → main (merge, delete branch)
 │
 └─── main (updated)
```

### Branch Protection Rules

The `main` branch has the following protections:
- Require pull request reviews (minimum 1 approval)
- Require status checks to pass before merging
- Require branches to be up to date before merging
- No direct commits allowed

## Commit Message Convention

We use **Conventional Commits** specification for clear, structured commit messages.

### Format

```
<type>(<scope>): <subject>

[optional body]

[optional footer]
```

### Types

- **feat**: A new feature
- **fix**: A bug fix
- **docs**: Documentation only changes
- **style**: Code style changes (formatting, missing semicolons, etc.)
- **refactor**: Code change that neither fixes a bug nor adds a feature
- **perf**: Performance improvements
- **test**: Adding or updating tests
- **chore**: Maintenance tasks, dependency updates
- **ci**: CI/CD configuration changes
- **build**: Build system or external dependency changes

### Scopes

Use these scopes to indicate which part of the codebase is affected:

- **packages**: Changes to IS packages (`/packages/**`)
- **config**: Configuration changes (`/config/**`)
- **docker**: Docker-related changes (`/docker/**`)
- **ci**: CI/CD workflow changes (`/.github/**`)
- **tests**: Test-related changes (`/tests/**`)
- **scripts**: Build/deployment script changes (`/scripts/**`)
- **docs**: Documentation changes (`/docs/**`)
- **helm**: Helm chart changes (`/helm/**`)

### Examples

```bash
# Adding a new feature
feat(packages): add order processing service

# Fixing a bug
fix(config): correct database connection string for test environment

# Updating documentation
docs(readme): add instructions for local development setup

# CI/CD changes
ci(workflows): add integration test stage to pipeline

# Configuration changes
chore(config): update prod environment variables

# Multiple scopes (use comma)
feat(packages,config): add customer service with dev config

# Breaking change (add ! after type/scope)
feat(packages)!: redesign authentication flow

BREAKING CHANGE: Authentication now requires OAuth2 tokens instead of basic auth
```

### Commit Message Guidelines

1. **Subject line**:
   - Use imperative mood ("add" not "added" or "adds")
   - Don't capitalize first letter after colon
   - No period at the end
   - Keep under 72 characters

2. **Body** (optional):
   - Separate from subject with blank line
   - Explain *what* and *why*, not *how*
   - Wrap at 72 characters

3. **Footer** (optional):
   - Reference issues: `Closes #123`, `Fixes #456`
   - Note breaking changes: `BREAKING CHANGE: description`

### Bad Examples ❌

```bash
# Too vague
git commit -m "update files"

# Missing type and scope
git commit -m "Added new service"

# Wrong mood
git commit -m "feat(packages): added payment service"

# Capitalized after colon
git commit -m "fix(config): Fix database connection"
```

### Good Examples ✅

```bash
# Clear and concise
git commit -m "feat(packages): add payment processing service"

# With body
git commit -m "fix(config): correct JDBC URL for test environment

The previous URL was pointing to the dev database, causing
test failures. Updated to use the correct test database endpoint."

# With issue reference
git commit -m "fix(packages): resolve null pointer in order validation

Closes #234"

# Breaking change
git commit -m "feat(packages)!: migrate to new authentication API

BREAKING CHANGE: All services must now use OAuth2 tokens.
Basic authentication is no longer supported."
```

## Development Workflow

### 1. Create a Feature Branch

```bash
# Ensure main is up to date
git checkout main
git pull origin main

# Create and switch to feature branch
git checkout -b feature/your-feature-name
```

### 2. Make Changes

- Write code following project conventions
- Add or update tests
- Update documentation as needed
- Commit frequently with clear messages

```bash
# Stage changes
git add <files>

# Commit with conventional message
git commit -m "feat(packages): add new feature"
```

### 3. Keep Branch Updated

```bash
# Regularly sync with main
git checkout main
git pull origin main
git checkout feature/your-feature-name
git rebase main
```

### 4. Push Changes

```bash
# Push feature branch
git push origin feature/your-feature-name
```

### 5. Create Pull Request

- Go to GitHub and create a pull request
- Fill out the PR template completely
- Link related issues
- Request reviews from code owners

## Pull Request Process

### Before Creating a PR

- [ ] All tests pass locally (`make test`)
- [ ] Code follows project style guidelines
- [ ] Documentation is updated
- [ ] Commit messages follow Conventional Commits
- [ ] Branch is up to date with `main`

### PR Title

Use the same format as commit messages:

```
feat(packages): add order processing service
```

### PR Description Template

```markdown
## Description
Brief description of changes

## Type of Change
- [ ] Bug fix (non-breaking change which fixes an issue)
- [ ] New feature (non-breaking change which adds functionality)
- [ ] Breaking change (fix or feature that would cause existing functionality to not work as expected)
- [ ] Documentation update

## Related Issues
Closes #123

## Testing
- [ ] Unit tests added/updated
- [ ] Integration tests added/updated
- [ ] Manual testing completed

## Checklist
- [ ] Code follows project conventions
- [ ] Self-review completed
- [ ] Documentation updated
- [ ] No new warnings generated
- [ ] Tests pass locally
```

### Review Process

1. **Automated Checks**: CI/CD pipeline must pass
2. **Code Review**: At least one approval required
3. **Code Owner Review**: Required for specific paths (see CODEOWNERS)
4. **Address Feedback**: Make requested changes
5. **Final Approval**: Merge when all checks pass and approved

### Merging

- Use **Squash and Merge** for feature branches
- Ensure final commit message follows Conventional Commits
- Delete branch after merge

## Code Review Guidelines

### For Authors

- Keep PRs small and focused (< 400 lines changed)
- Provide context in PR description
- Respond to feedback promptly
- Be open to suggestions

### For Reviewers

- Review within 24 hours
- Be constructive and respectful
- Focus on:
  - Correctness and logic
  - Test coverage
  - Security implications
  - Performance impact
  - Code maintainability
- Approve when satisfied, request changes if needed

## Testing Requirements

### Unit Tests

- Required for all new features
- Must maintain or improve code coverage
- Located in `tests/unit/`

### Integration Tests

- Required for API changes
- Must test end-to-end workflows
- Located in `tests/integration/`

### Running Tests Locally

```bash
# Run all tests
make test

# Run specific test suite
./scripts/run-unit-tests.sh PackageName
./scripts/run-integration-tests.sh api-tests
```

## Code Style

### General Guidelines

- Follow language-specific conventions (Java, JavaScript, etc.)
- Use meaningful variable and function names
- Keep functions small and focused
- Add comments for complex logic
- Remove commented-out code

### Configuration Files

- Use consistent indentation (see `.editorconfig`)
- Keep files organized and well-structured
- Document non-obvious settings

### Docker Files

- Use multi-stage builds
- Minimize layer count
- Follow security best practices
- Document build arguments

## Getting Help

If you need assistance:

1. Check existing documentation in `/docs`
2. Review closed PRs for similar changes
3. Ask in team chat or discussion forum
4. Tag relevant code owners in your PR

## Code of Conduct

- Be respectful and professional
- Welcome newcomers
- Focus on constructive feedback
- Assume good intentions

## License

By contributing, you agree that your contributions will be licensed under the same license as the project.

---

Thank you for contributing! 🎉
