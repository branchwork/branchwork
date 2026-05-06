# Corporate webMethods Microservices Runtime (MSR) Base Image
# This image serves as the foundation for all microservice images

FROM softwareag/webmethods-microservicesruntime:10.15

LABEL maintainer="integration-team@company.com"
LABEL description="Corporate MSR base image with security patches and common libraries"

# Set environment variables
ENV SAG_HOME=/opt/softwareag
ENV JAVA_MIN_MEM=512m
ENV JAVA_MAX_MEM=2048m

# Switch to root for system-level installations
USER root

# Install security patches and utilities
RUN apt-get update && \
    apt-get upgrade -y && \
    apt-get install -y --no-install-recommends \
        curl \
        wget \
        jq \
        netcat-openbsd \
        dnsutils && \
    apt-get clean && \
    rm -rf /var/lib/apt/lists/*

# Create directories for logs and configuration
RUN mkdir -p ${SAG_HOME}/IntegrationServer/logs && \
    mkdir -p ${SAG_HOME}/IntegrationServer/config && \
    mkdir -p ${SAG_HOME}/common/conf

# Copy common libraries (if any)
# COPY libs/*.jar ${SAG_HOME}/IntegrationServer/lib/jars/custom/

# Set up monitoring agent (placeholder - adjust based on your monitoring solution)
# COPY monitoring-agent/ ${SAG_HOME}/monitoring/

# Configure JVM options for production
RUN echo "-Xms${JAVA_MIN_MEM}" >> ${SAG_HOME}/IntegrationServer/bin/setenv.sh && \
    echo "-Xmx${JAVA_MAX_MEM}" >> ${SAG_HOME}/IntegrationServer/bin/setenv.sh && \
    echo "-XX:+UseG1GC" >> ${SAG_HOME}/IntegrationServer/bin/setenv.sh && \
    echo "-XX:+HeapDumpOnOutOfMemoryError" >> ${SAG_HOME}/IntegrationServer/bin/setenv.sh && \
    echo "-XX:HeapDumpPath=${SAG_HOME}/IntegrationServer/logs" >> ${SAG_HOME}/IntegrationServer/bin/setenv.sh

# Set proper permissions
RUN chown -R sagadmin:sagadmin ${SAG_HOME}/IntegrationServer/logs && \
    chown -R sagadmin:sagadmin ${SAG_HOME}/IntegrationServer/config && \
    chmod -R 755 ${SAG_HOME}/IntegrationServer/logs

# Switch back to sagadmin user
USER sagadmin

# Health check
HEALTHCHECK --interval=30s --timeout=10s --start-period=60s --retries=3 \
    CMD curl -f http://localhost:5555/health || exit 1

# Expose ports
EXPOSE 5555 9999

# Default command
CMD ["${SAG_HOME}/IntegrationServer/bin/startup.sh"]
