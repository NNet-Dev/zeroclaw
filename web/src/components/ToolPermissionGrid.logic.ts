export const NONE_SENTINEL = '__none__';
export const APPROVAL_WILDCARD = '*';

export type AuthState = 'deny' | 'inherit' | 'allow';
export type ApprState = 'ask' | 'inherit' | 'auto';
export type CustomPermissionTarget = AuthState | ApprState;

export interface ToolPermissionGridValue {
  allowedTools: string[];
  excludedTools: string[];
  autoApprove: string[];
  alwaysAsk: string[];
}

export function realAllowedTools(allowedTools: string[]): string[] {
  return allowedTools.filter((name) => name !== NONE_SENTINEL);
}

export function isMcpToolName(name: string): boolean {
  return name !== NONE_SENTINEL && name.includes('__');
}

export function effectiveAuthState({
  name,
  strict,
  realAllowSet,
  excludedSet,
}: {
  name: string;
  strict: boolean;
  realAllowSet: ReadonlySet<string>;
  excludedSet: ReadonlySet<string>;
}): AuthState {
  if (excludedSet.has(name)) return 'deny';
  if (strict && (realAllowSet.has(name) || isMcpToolName(name))) return 'allow';
  return 'inherit';
}

export function isMcpAutoAdmitted({
  name,
  strict,
  realAllowSet,
  excludedSet,
}: {
  name: string;
  strict: boolean;
  realAllowSet: ReadonlySet<string>;
  excludedSet: ReadonlySet<string>;
}): boolean {
  return (
    strict &&
    isMcpToolName(name) &&
    !realAllowSet.has(name) &&
    !excludedSet.has(name)
  );
}

export function effectiveApprovalState({
  name,
  autoApproveSet,
  alwaysAskSet,
}: {
  name: string;
  autoApproveSet: ReadonlySet<string>;
  alwaysAskSet: ReadonlySet<string>;
}): ApprState {
  if (alwaysAskSet.has(APPROVAL_WILDCARD) || alwaysAskSet.has(name)) return 'ask';
  if (autoApproveSet.has(APPROVAL_WILDCARD) || autoApproveSet.has(name)) return 'auto';
  return 'inherit';
}

export function isAlwaysAskWildcardLocked({
  name,
  alwaysAskSet,
}: {
  name: string;
  alwaysAskSet: ReadonlySet<string>;
}): boolean {
  return name !== APPROVAL_WILDCARD && alwaysAskSet.has(APPROVAL_WILDCARD);
}

export function applyAuthState(
  value: ToolPermissionGridValue,
  name: string,
  next: AuthState,
  strict: boolean,
): ToolPermissionGridValue {
  const nextExcluded = value.excludedTools.filter((item) => item !== name);
  const nextRealAllow = new Set(realAllowedTools(value.allowedTools));
  nextRealAllow.delete(name);

  if (next === 'deny') {
    nextExcluded.push(name);
  } else if (next === 'allow') {
    nextRealAllow.add(name);
  }

  const nextAllowedTools = strict
    ? nextRealAllow.size > 0
      ? [...nextRealAllow]
      : [NONE_SENTINEL]
    : [];

  return {
    ...value,
    excludedTools: nextExcluded,
    allowedTools: nextAllowedTools,
  };
}

export function applyApprovalState(
  value: ToolPermissionGridValue,
  name: string,
  next: ApprState,
): ToolPermissionGridValue {
  const nextAlwaysAsk = value.alwaysAsk.filter((item) => item !== name);
  const nextAutoApprove = value.autoApprove.filter((item) => item !== name);

  if (next === 'ask') nextAlwaysAsk.push(name);
  else if (next === 'auto') nextAutoApprove.push(name);

  return {
    ...value,
    alwaysAsk: nextAlwaysAsk,
    autoApprove: nextAutoApprove,
  };
}

export function applyStrictMode(
  value: ToolPermissionGridValue,
  nextStrict: boolean,
): ToolPermissionGridValue {
  const nextRealAllow = realAllowedTools(value.allowedTools);
  return {
    ...value,
    allowedTools: nextStrict
      ? nextRealAllow.length > 0
        ? nextRealAllow
        : [NONE_SENTINEL]
      : [],
  };
}

export function applyCustomPermission(
  value: ToolPermissionGridValue,
  rawName: string,
  target: CustomPermissionTarget,
): ToolPermissionGridValue | null {
  const name = rawName.trim();
  if (name.length === 0 || name === NONE_SENTINEL) return null;

  if (target === 'deny') {
    return applyAuthState(value, name, 'deny', value.allowedTools.length > 0);
  }
  if (target === 'allow') {
    return {
      ...applyAuthState(value, name, 'allow', true),
      excludedTools: value.excludedTools.filter((item) => item !== name),
    };
  }
  if (target === 'ask') {
    return applyApprovalState(value, name, 'ask');
  }

  return applyApprovalState(value, name, 'auto');
}
