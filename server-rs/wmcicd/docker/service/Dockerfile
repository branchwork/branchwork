# Microservice-specific Docker image
# Derives from corporate MSR base image and includes IS packages

ARG BASE_IMAGE=corporate-msr-base:latest
FROM ${BASE_IMAGE}

LABEL maintainer="integration-team@company.com"
LABEL description="Microservice image with IS packages and environment configuration"

# Build arguments
ARG ENVIRONMENT=dev
ARG SERVICE_NAME=microservice
ARG VERSION=latest

# Environment variables
ENV SERVICE_NAME=${SERVICE_NAME}
ENV ENVIRONMENT=${ENVIRONMENT}
ENV VERSION=${VERSION}

# Switch to root for file operations
USER root

# Copy IS packages
COPY --chown=sagadmin:sagadmin packages/ ${SAG_HOME}/IntegrationServer/packages/

# Copy environment-specific configuration
COPY --chown=sagadmin:sagadmin config/base/ ${SAG_HOME}/IntegrationServer/config/base/
COPY --chown=sagadmin:sagadmin config/${ENVIRONMENT}/ ${SAG_HOME}/IntegrationServer/config/${ENVIRONMENT}/

# Copy configuration application script
COPY --chown=sagadmin:sagadmin scripts/apply-config.sh /tmp/apply-config.sh
RUN chmod +x /tmp/apply-config.sh

# Apply configuration for the target environment
RUN /tmp/apply-config.sh ${ENVIRONMENT} && \
    rm /tmp/apply-config.sh

# Ensure proper permissions on packages
RUN chown -R sagadmin:sagadmin ${SAG_HOME}/IntegrationServer/packages && \
    chmod -R 755 ${SAG_HOME}/IntegrationServer/packages

# Switch back to sagadmin user
USER sagadmin

# Add service-specific labels
LABEL service.name="${SERVICE_NAME}"
LABEL service.version="${VERSION}"
LABEL service.environment="${ENVIRONMENT}"

# Health check specific to this service
HEALTHCHECK --interval=30s --timeout=10s --start-period=90s --retries=3 \
    CMD curl -f http://localhost:5555/health || exit 1

# Expose service ports
EXPOSE 5555 9999

# Start Integration Server
CMD ["${SAG_HOME}/IntegrationServer/bin/startup.sh"]
