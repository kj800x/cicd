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
   - Improved object model with proper type hierarchy
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

### GraphQL Schema

The API now features a more logical object hierarchy:

```graphql
# Repository information
type Repository {
  id: ID!
  name: String!
  owner: String!
  defaultBranch: String!
  isPrivate: Boolean!
  language: String
}

# Git commit information
type Commit {
  id: ID!
  sha: String!
  message: String!
  timestamp: Int!
  author: String!
  parentShas: [String!]!
}

# Branch information
type Branch {
  id: ID!
  name: String!
  headCommitSha: String!
}

# Build information (CI/CD)
type Build {
  commit: Commit!
  repository: Repository!
  status: String!
  url: String
  branches: [Branch!]!
}

# Root query type
type Query {
  # Get builds from the last hour
  recentBuilds: [Build!]!

  # Find a specific build by commit SHA
  build(sha: String!): Build

  # Get parent builds of a specific commit
  parentBuilds(sha: String!, maxDepth: Int): [Build!]!

  # Get child builds of a specific commit
  childBuilds(sha: String!): [Build!]!

  # Get all repositories
  repositories: [Repository!]!

  # Get branches for a repository
  branches(repoId: ID!): [Branch!]!
}
```

### Example Queries

```graphql
# Get recent builds
query RecentBuilds {
  recentBuilds {
    commit {
      sha
      message
      timestamp
      author
      parentShas
    }
    repository {
      name
      owner
    }
    status
    url
    branches {
      name
    }
  }
}

# Get a specific build
query GetBuild {
  build(sha: "abc123") {
    commit {
      sha
      message
    }
    repository {
      name
      owner
    }
    status
    branches {
      name
    }
  }
}

# Trace commit history (parents)
query ParentBuilds {
  parentBuilds(sha: "abc123", maxDepth: 10) {
    commit {
      sha
      message
      parentShas
    }
    repository {
      name
    }
    status
  }
}

# Find child builds
query ChildBuilds {
  childBuilds(sha: "abc123") {
    commit {
      sha
      message
    }
    status
    repository {
      name
      owner
    }
  }
}

# Get all repositories
query Repositories {
  repositories {
    id
    name
    owner
    defaultBranch
    isPrivate
    language
  }
}

# Get branches for a repository
query Branches {
  branches(repoId: "123") {
    id
    name
    headCommitSha
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
