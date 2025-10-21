# Deploy Action

To do a deploy action (artifactSha, configSha)
- User provides a config sha
- User provides an optional artifactSha

A deploy request is fully defined by (configName, artifactSha, configSha)?

### Problems

(None, None) - means what DeployConfig CRD state in Kube? (what specs are included?) Do we just replace it with a virtual state? 
(None, Sha ) - currently means undeploy, but it would also mean a valid deploy state for artifactless configs.
(Sha , None) - Invalid state (fine)
(Sha , Sha ) - State only for artifactful configs.

When we update the deploy config object in kube, we just update desired artifactSha and let the reconsiler resolve it. When we update the config for the object, we edit it directly. Is this how we want things to be?

Maybe we fix (None, None) to be the undeploy state. We blank out specs when we enter that state. (None, Sha) is only for artifactless configs. (Sha, Sha) is only for artifactful configs. 

# Virtual Deploy Config

A virtual deploy config is basically the (None, None) state. It has the fields in the root file of the deploy config, but it does not have the specs, the specs are just an empty array. 

### Features needed

- Deploy artifacts
- Track branches
- Deploy artifact-less configs.
- Autodeploy
- Segment by team
- Show deployment status
- Track deployment history
- Ability to re-apply a previous deployment (artifactSha and configSha).

# Webhook Processing

## Push Event

- Upsert repo
- Upsert branch  
- Upsert commit
- Run autodeploy (only for artifactless configs)
- If master, sync configs (create virtual configs for new ones, kube remove any ones which are now missing)

## Build Start Event

- Update commit status in db

## Build Completed Event

- Update commit status in db
- If success, run autodeploy (only for artifactful configs)

## Global sync (not a webhook but an admin endpoint)

Hit this endpoint to sync with all repos.

- Scan through all repos available, add Repos to db.
- Scan through all branches available, add branches to db.
- Scan through all HEAD commits, add commits to db (including commit build statuses).
- Sync all deploy configs with kube (create virtual configs for configs not existing, undeploy configs that are no longer found).

# Autodeploy Behavior

 Complications:
 - Artifactless vs artifactful
 - Undeploy state
 - Branch deploys
 - Read deploy configs from branches?
 - Deploy configs where they live in different repos than the artifact repo. 
