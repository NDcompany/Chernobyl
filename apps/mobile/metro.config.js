const { getDefaultConfig } = require('expo/metro-config');
const path = require('path');

// npm workspaces monorepo: Metro needs the workspace root as a watch folder
// so it can follow the @chernobyl/protocol symlink.
const projectRoot = __dirname;
const workspaceRoot = path.resolve(projectRoot, '../..');

const config = getDefaultConfig(projectRoot);
config.watchFolders = [workspaceRoot];
config.resolver.nodeModulesPaths = [
  path.resolve(projectRoot, 'node_modules'),
  path.resolve(workspaceRoot, 'node_modules'),
];

module.exports = config;
