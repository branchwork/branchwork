# webMethods Microservice CI/CD Pipeline

This repository contains the source code, configuration, and deployment artifacts for webMethods Integration Server (IS) microservices with a complete CI/CD pipeline on GitHub Actions.

## Repository Structure

```
wmcicd/
├── packages/                  # Integration Server packages
│   └── <PackageName>/         # Each IS package (ns/, code/, manifest.v3, etc.)
├── config/                    # Environment-specific configurations
│   ├── base/                  # Default configurations shared by all environments
│   ├── dev/                   # Development environment overlays
│   ├── test/                  # Test environment overlays
│   └── prod/                  # Production environment overlays
├── tests/                     # Test suites
│   ├── unit/                  # Unit tests (wm-jbehave .story files + step definitions)
│   └── integration/           # Integration tests (newman/k6/REST-assured suites)
├── docker/                    # Docker build configurations
│   ├── base/                  # Corporate MSR base image
│   └── service/               # Per-microservice image (derives from base)
├── helm/                      # Kubernetes Helm charts (optional)
├── scripts/                   # Shell helpers used by CI (apply-config.sh, etc.)
├── .github/workflows/         # GitHub Actions CI/CD workflows
└── docs/                      # Additional documentation
```

## Getting Started

### Prerequisites

- webMethods Integration Server 10.x or later
- Docker (for containerization)
- Git
- Access to corporate artifact repository

### Development Workflow

1. **Clone the repository**
   ```bash
   git clone <repository-url>
   cd wmcicd
   ```

2. **Add or modify IS packages**
   - Place IS packages in the `packages/` directory
   - Follow the standard IS package structure (ns/, code/, manifest.v3)

3. **Configure environments**
   - Base configurations go in `config/base/`
   - Environment-specific overrides go in `config/{dev,test,prod}/`

4. **Write tests**
   - Unit tests in `tests/unit/`
   - Integration tests in `tests/integration/`

5. **Commit and push**
   - Changes trigger the CI/CD pipeline automatically

## CI/CD Pipeline

The GitHub Actions workflows in `.github/workflows/` handle:

- Building Docker images
- Running unit and integration tests
- Deploying to target environments
- Promoting releases through environments (dev → test → prod)

## Configuration Management

Configuration files are organized by environment:

- **base/**: Default values and common settings
- **dev/**: Development-specific overrides
- **test/**: Test environment settings
- **prod/**: Production settings

The `scripts/apply-config.sh` helper merges base + environment-specific configs during deployment.

## Docker Images

### Base Image (`docker/base/`)
Corporate-standard MSR base image with:
- Security patches
- Common libraries
- Monitoring agents

### Service Image (`docker/service/`)
Microservice-specific image that:
- Derives from base image
- Includes IS packages
- Applies environment configuration

## Testing

### Unit Tests
Located in `tests/unit/`, using wm-jbehave framework:
```bash
# Run unit tests
./scripts/run-unit-tests.sh
```

### Integration Tests
Located in `tests/integration/`, using newman/k6/REST-assured:
```bash
# Run integration tests
./scripts/run-integration-tests.sh
```

## Contributing

1. Create a feature branch from `main`
2. Make your changes
3. Ensure tests pass
4. Submit a pull request
5. Code owners will review (see CODEOWNERS)

## File Conventions

- **Line endings**: LF (Unix-style) for all text files
- **Encoding**: UTF-8
- **Indentation**: See `.editorconfig` for language-specific rules

## Ignored Files

The following are excluded from version control (see `.gitignore`):
- IS work directories (`work/`, `logs/`)
- Build artifacts (`*.zip`, `*.jar` except tracked dependencies)
- IDE configuration files
- Temporary files

## Support

For questions or issues:
- Check the `docs/` directory for detailed documentation
- Contact the integration team (see CODEOWNERS)
- Open an issue in this repository

## License

[Specify your license here]
