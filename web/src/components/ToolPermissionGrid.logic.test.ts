import assert from 'node:assert/strict';
import test from 'node:test';

import {
  APPROVAL_WILDCARD,
  applyCustomPermission,
  effectiveApprovalState,
  effectiveAuthState,
  isMcpAutoAdmitted,
  type ToolPermissionGridValue,
} from './ToolPermissionGrid.logic.ts';

function sets(value: ToolPermissionGridValue) {
  return {
    realAllowSet: new Set(value.allowedTools.filter((name) => name !== '__none__')),
    excludedSet: new Set(value.excludedTools),
    autoApproveSet: new Set(value.autoApprove),
    alwaysAskSet: new Set(value.alwaysAsk),
  };
}

test('strict allowlists still auto-admit MCP names unless denied', () => {
  const value: ToolPermissionGridValue = {
    allowedTools: ['__none__'],
    excludedTools: [],
    autoApprove: [],
    alwaysAsk: [],
  };
  const current = sets(value);

  assert.equal(
    effectiveAuthState({
      name: 'server__tool',
      strict: true,
      realAllowSet: current.realAllowSet,
      excludedSet: current.excludedSet,
    }),
    'allow',
  );
  assert.equal(
    isMcpAutoAdmitted({
      name: 'server__tool',
      strict: true,
      realAllowSet: current.realAllowSet,
      excludedSet: current.excludedSet,
    }),
    true,
  );
  assert.equal(
    effectiveAuthState({
      name: 'shell',
      strict: true,
      realAllowSet: current.realAllowSet,
      excludedSet: current.excludedSet,
    }),
    'inherit',
  );

  const denied = sets({ ...value, excludedTools: ['server__tool'] });
  assert.equal(
    effectiveAuthState({
      name: 'server__tool',
      strict: true,
      realAllowSet: denied.realAllowSet,
      excludedSet: denied.excludedSet,
    }),
    'deny',
  );
});

test('approval wildcards follow runtime precedence', () => {
  const askWildcard = sets({
    allowedTools: [],
    excludedTools: [],
    autoApprove: [APPROVAL_WILDCARD, 'shell'],
    alwaysAsk: [APPROVAL_WILDCARD],
  });

  assert.equal(
    effectiveApprovalState({
      name: 'shell',
      autoApproveSet: askWildcard.autoApproveSet,
      alwaysAskSet: askWildcard.alwaysAskSet,
    }),
    'ask',
  );

  const autoWildcard = sets({
    allowedTools: [],
    excludedTools: [],
    autoApprove: [APPROVAL_WILDCARD],
    alwaysAsk: [],
  });
  assert.equal(
    effectiveApprovalState({
      name: 'shell',
      autoApproveSet: autoWildcard.autoApproveSet,
      alwaysAskSet: autoWildcard.alwaysAskSet,
    }),
    'auto',
  );

  const exactAskOverridesAutoWildcard = sets({
    allowedTools: [],
    excludedTools: [],
    autoApprove: [APPROVAL_WILDCARD],
    alwaysAsk: ['shell'],
  });
  assert.equal(
    effectiveApprovalState({
      name: 'shell',
      autoApproveSet: exactAskOverridesAutoWildcard.autoApproveSet,
      alwaysAskSet: exactAskOverridesAutoWildcard.alwaysAskSet,
    }),
    'ask',
  );
});

test('custom names can be added to each permission state', () => {
  const base: ToolPermissionGridValue = {
    allowedTools: [],
    excludedTools: [],
    autoApprove: [],
    alwaysAsk: [],
  };

  assert.deepEqual(applyCustomPermission(base, 'worker__dynamic', 'deny')?.excludedTools, [
    'worker__dynamic',
  ]);
  assert.deepEqual(applyCustomPermission(base, 'worker__dynamic', 'allow')?.allowedTools, [
    'worker__dynamic',
  ]);
  assert.deepEqual(applyCustomPermission(base, 'worker__dynamic', 'ask')?.alwaysAsk, [
    'worker__dynamic',
  ]);
  assert.deepEqual(applyCustomPermission(base, 'worker__dynamic', 'auto')?.autoApprove, [
    'worker__dynamic',
  ]);
});
