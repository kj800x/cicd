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
- Track parent-child relationships between commits, including merge commits with multiple parents
- Track which commits belong to which branches
- GraphQL API to query recent builds with branch information and commit history
- Metrics endpoint for monitoring

## Enhanced Git Model

The application now features an enhanced git model that tracks:

1. **Branch Information**:
   - Which commits belong to which branches (many-to-many relationship)
   - Head commit SHAs for branches

2. **Commit Relationships**:
   - Parent-child relationships between commits
   - Support for multiple parents (merge commits)
   - Ability to trace commit history in both directions (parents and children)

3. **GraphQL API Enhancements**:
   - Query recent builds with branch information
   - Look up a specific commit by SHA with branch details
   - Retrieve parent commits to trace history
   - Find child commits that descend from a specific commit

## Planned Improvements

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

### GraphQL Queries

The API now supports the following queries:

```graphql
# Get recent builds with basic info
query RecentBuilds {
  recentBuilds {
    id
    sha
    message
    timestamp
    buildStatus
    buildUrl
    parentShas
    repoName
    repoOwnerName
  }
}

# Get recent builds with branch information
query RecentBuildsWithBranches {
  recentBuildsWithBranches {
    id
    sha
    message
    timestamp
    buildStatus
    buildUrl
    parentShas
    repoName
    repoOwnerName
    branches {
      id
      name
      headCommitSha
    }
  }
}

# Look up a specific commit by SHA
query Commit {
  commit(sha: "abc123") {
    id
    sha
    message
    timestamp
    buildStatus
    buildUrl
    parentShas
    repoName
    repoOwnerName
    branches {
      id
      name
      headCommitSha
    }
  }
}

# Trace commit history (parents)
query ParentCommits {
  parentCommits(sha: "abc123", maxDepth: 10) {
    id
    sha
    message
    timestamp
    buildStatus
    buildUrl
    parentShas
    repoName
    repoOwnerName
  }
}

# Find child commits
query CommitChildren {
  commitChildren(sha: "abc123") {
    id
    sha
    message
    timestamp
    buildStatus
    buildUrl
    parentShas
    repoName
    repoOwnerName
  }
}
```

## GitHub Webhook Integration

The application receives GitHub webhook events through a websocket proxy service. The current events being processed are:
- Push events - For tracking new commits and branch updates
- Check run events - For tracking build status

## Database Schema

The application uses SQLite with the following tables:
- `git_repo`: Stores repository information
- `git_branch`: Tracks branches and their head commits
- `git_commit`: Stores commit information and build status
- `git_commit_branch`: Junction table tracking which commits belong to which branches
- `git_commit_parent`: Junction table tracking parent-child relationships between commits

## Future Roadmap

1. Discord Integration:
   - Send notifications about build status changes
   - Configurable alerts for build failures

2. Kubernetes Integration:
   - Track container deployments
   - Identify pods running outdated builds
   - Provide recommendations for updates

3. Multi-user Support:
   - Track repositories across multiple GitHub users
   - Role-based access controls for the dashboard

## License

[Add appropriate license information here]
