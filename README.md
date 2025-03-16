# CI/CD Build Status Dashboard

A dashboard application for monitoring GitHub build statuses with real-time updates. This project tracks build status and related information for repositories hosted on GitHub that use GitHub Actions for CI/CD.

## Project Overview

This dashboard monitors repositories that:
- Are hosted on GitHub
- Use GitHub Actions for building
- Publish packages to GitHub Container Registry (GHCR)
- Are a mix of public and private repositories
- Currently owned by one user, with plans to support multiple users

The application receives GitHub webhook events through a websocket proxy, as the server is hosted internally and not exposed to the internet.

## Current Features

- Track repository information (owner, name, privacy status, language, default branch)
- Monitor commits (SHA, message, timestamp)
- Track build status (None, Pending, Success, Failure)
- Store branch information and head commit references
- GraphQL API to query recent builds
- Metrics endpoint for monitoring

## Planned Improvements

- Enhanced Git model with more detailed tracking information
- Better branch tracking for builds and commits
- Discord status notifications
- Kubernetes integration to detect when pods are running outdated builds
- Support for repositories owned by multiple users

## Architecture

The application is built using:
- **Rust** with Actix Web for the HTTP server
- **SQLite** for storing repository, commit, and build data
- **GraphQL** for API queries
- **WebSockets** for receiving GitHub webhook events
- **Prometheus** for metrics

## Setup and Configuration

### Environment Variables

The application requires the following environment variables:

- `WEBSOCKET_URL`: URL for the websocket proxy that forwards GitHub webhooks
- `CLIENT_SECRET`: Secret for authenticating with the websocket proxy
- `DATABASE_PATH`: (Optional) Path to the SQLite database file (defaults to "db.db")

### Running the Application

#### Using Docker

```bash
docker build -t cicd-dashboard .
docker run -p 8080:8080 \
  -e WEBSOCKET_URL=your_websocket_url \
  -e CLIENT_SECRET=your_client_secret \
  -v /path/to/data:/app/data \
  cicd-dashboard
```

#### Locally

```bash
cargo run
```

## API Endpoints

- `/api/graphql`: GraphQL API endpoint for querying data
- `/api/metrics`: Prometheus metrics endpoint

## GitHub Webhook Integration

The application receives GitHub webhook events through a websocket proxy service. The current events being processed are:
- Push events - For tracking new commits
- Check run events - For tracking build status

## Database Schema

The application uses SQLite with the following tables:
- `git_repo`: Stores repository information
- `git_branch`: Tracks branches and their head commits
- `git_commit`: Stores commit information and build status

## Future Roadmap

1. Enhanced Git Model:
   - Improve branch tracking for builds
   - Capture more metadata about commits and builds

2. Discord Integration:
   - Send notifications about build status changes
   - Configurable alerts for build failures

3. Kubernetes Integration:
   - Track container deployments
   - Identify pods running outdated builds
   - Provide recommendations for updates

4. Multi-user Support:
   - Track repositories across multiple GitHub users
   - Role-based access controls for the dashboard

## License

[Add appropriate license information here]
