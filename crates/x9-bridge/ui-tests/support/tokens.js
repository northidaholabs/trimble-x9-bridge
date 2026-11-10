// Test-only tokens, written to a temporary tokens file for the bridge.
export const TOKENS = {
  viewer: "viewer-token-ui-0123456789",
  agent: "agent-token-ui-0123456789",
  operator: "operator-token-ui-0123456789",
};
// Names in the tokens file are `<key>-1`; the role is the key unless given here.
export const ROLES = {};
export const nameOf = (who) => `${who}-1`;
