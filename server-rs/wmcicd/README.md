# webMethods Microservice CI/CD Pipeline

This repository contains the source code, configuration, and deployment artifacts for webMethods Integration Server (IS) microservices with a complete CI/CD pipeline on GitHub Actions.

## Prerequisites

Before you begin, ensure you have the following installed:

- **Docker** (20.10+) - For building and running containers
- **Java 17** (JDK) - Required for building and testing IS packages
- **jq** - JSON processor for configuration management
- **curl** - For API calls and health checks
- **act** (optional) - For running GitHub Actions workflows locally
- **make** - For using the build automation commands
- **Git** - Version control

### Optional Tools

- **wm-mcp-server** - webMethods Model Context Protocol server for AI-assisted development
  - Documentation: [wm-mcp-server docs](https://github.com/your-org/wm-mcp-server)

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

## Quick Start Commands

Use these `make` commands for common tasks:

```bash
# Build all packages
make build

# Run unit tests
make test

# Build Docker image
make image

# Deploy to an environment (dev, test, or prod)
make deploy ENV=dev
make deploy ENV=test
make deploy ENV=prod
```

## Getting Started

### 1. Clone the Repository

```bash
git clone <repository-url>
cd wmcicd
```

### 2. Build Locally

```bash
# Build all IS packages
make build

# Run tests
make test
```

### 3. Run the Pipeline Locally

You have two options for running the CI/CD pipeline locally:

#### Option A: Using `act` (GitHub Actions locally)

```bash
# Install act if you haven't already
# macOS: brew install act
# Linux: see https://github.com/nektos/act

# Run the entire workflow
act

# Run a specific job
act -j build

# Run with specific event
act push
```

#### Option B: Using Docker Compose

```bash
# Start the full local development stack
docker compose up

# Build and start in detached mode
docker compose up -d

# View logs
docker compose logs -f

# Stop the stack
docker compose down
```

## How to Add a New Package

1. **Create the package directory structure**:
   ```bash
   mkdir -p packages/YourPackageName/{ns,code}
   ```

2. **Add your package files**:
   - Place flow services in `packages/YourPackageName/ns/`
   - Place Java services in `packages/YourPackageName/code/`
   - Create `manifest.v3` with package metadata

3. **Add package configuration**:
   - Add base configuration in `config/base/YourPackageName.properties`
   - Add environment-specific overrides in `config/{dev,test,prod}/`

4. **Write tests**:
   - Add unit tests in `tests/unit/YourPackageName/`
   - Add integration tests in `tests/integration/YourPackageName/`

5. **Update documentation**:
   - Document your package in `docs/packages/YourPackageName.md`

6. **Commit and push**:
   ```bash
   git add packages/YourPackageName
   git commit -m "feat(packages): add YourPackageName"
   git push
   ```

## How to Add a New Environment

1. **Create environment configuration directory**:
   ```bash
   mkdir -p config/newenv
   ```

2. **Add environment-specific configurations**:
   - Copy relevant files from `config/base/` to `config/newenv/`
   - Modify values for the new environment
   - Add secrets/credentials references (never commit actual secrets)

3. **Update CI/CD workflow**:
   - Edit `.github/workflows/deploy.yml`
   - Add the new environment to the deployment matrix
   - Configure environment protection rules in GitHub

4. **Update deployment scripts**:
   - Modify `scripts/apply-config.sh` if needed
   - Add environment-specific deployment logic

5. **Document the environment**:
   - Update `docs/environments.md` with connection details
   - Document any environment-specific requirements

## Configuration Management

Configuration files are organized by environment:

- **base/**: Default values and common settings
- **dev/**: Development-specific overrides
- **test/**: Test environment settings
- **prod/**: Production settings

The `scripts/apply-config.sh` helper merges base + environment-specific configs during deployment.

### Configuration Precedence

```
base config < environment config < runtime overrides
```

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
# Run all unit tests
make test

# Run specific package tests
./scripts/run-unit-tests.sh YourPackageName
```

### Integration Tests
Located in `tests/integration/`, using newman/k6/REST-assured:
```bash
# Run all integration tests
make integration-test

# Run specific test suite
./scripts/run-integration-tests.sh api-tests
```

## CI/CD Pipeline

The GitHub Actions workflows in `.github/workflows/` handle:

- **Build**: Compile packages, run unit tests
- **Test**: Run integration tests
- **Image**: Build and push Docker images
- **Deploy**: Deploy to target environments
- **Promote**: Promote releases through environments (dev → test → prod)

### Pipeline Stages

1. **Build** - Triggered on every push
2. **Test** - Runs after successful build
3. **Deploy to Dev** - Automatic on main branch
4. **Deploy to Test** - Manual approval required
5. **Deploy to Prod** - Manual approval + additional checks

## Contributing

Please read [CONTRIBUTING.md](CONTRIBUTING.md) for details on:
- Branch model and workflow
- Commit message conventions
- Pull request process
- Code review guidelines

## Code Ownership

See [CODEOWNERS](CODEOWNERS) for automatic review assignment:
- `/packages/**` - Integration team and package maintainers
- `/config/**` - DevOps and integration teams
- `/.github/**` - DevOps team
- `/docker/**` - DevOps team

## Troubleshooting

### Common Issues

**Build fails with "package not found"**
- Ensure `manifest.v3` exists and is properly formatted
- Check that package dependencies are declared

**Tests fail locally but pass in CI**
- Verify Java version matches CI (Java 17)
- Check Docker version compatibility
- Ensure all environment variables are set

**Docker image build fails**
- Check base image availability
- Verify network connectivity to artifact repository
- Review `docker/service/Dockerfile` for syntax errors

**Deployment fails**
- Verify environment configuration files exist
- Check credentials and access permissions
- Review deployment logs in GitHub Actions

## Support

For questions or issues:
- Check the `docs/` directory for detailed documentation
- Review [CONTRIBUTING.md](CONTRIBUTING.md) for development guidelines
- Contact the integration team (see [CODEOWNERS](CODEOWNERS))
- Open an issue in this repository

## Additional Resources

- [wm-mcp-server documentation](https://github.com/your-org/wm-mcp-server) - AI-assisted development tools
- [webMethods Integration Server documentation](https://documentation.softwareag.com/)
- [GitHub Actions documentation](https://docs.github.com/en/actions)
- [Docker best practices](https://docs.docker.com/develop/dev-best-practices/)

## License

[Specify your license here]