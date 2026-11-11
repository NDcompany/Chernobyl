/**
 * Unit tests for design tokens. Run with `node --test src/theme/tokens.test.ts` from apps/mobile.
 */
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { TAB_BAR_HEIGHT, colors, radii, shadow, spacing, typography } from './tokens.ts';

test('colors: palette invariants and contrast primitives', () => {
  assert.equal(colors.black, '#050505');
  assert.equal(colors.white, '#FFFFFF');
  assert.match(colors.success, /^#[0-9A-Fa-f]{6}$/);
  assert.match(colors.warning, /^#[0-9A-Fa-f]{6}$/);
  assert.match(colors.error, /^#[0-9A-Fa-f]{6}$/);
  // restrained backgrounds are hex colors
  assert.match(colors.successBg, /^#[0-9A-Fa-f]{6}$/);
  assert.match(colors.warningBg, /^#[0-9A-Fa-f]{6}$/);
  assert.match(colors.errorBg, /^#[0-9A-Fa-f]{6}$/);
});

test('spacing: monotonic scale progression', () => {
  const values = [
    spacing.xs,
    spacing.sm,
    spacing.md,
    spacing.lg,
    spacing.xl,
    spacing.xxl,
    spacing.xxxl,
    spacing.huge,
  ];
  for (let i = 1; i < values.length; i++) {
    assert.ok(values[i] > values[i - 1], `spacing index ${i} must be greater than ${i - 1}`);
  }
  assert.equal(spacing.xs, 4);
  assert.equal(spacing.huge, 40);
});

test('radii: screen, containers and pill scale', () => {
  assert.equal(radii.pill, 999);
  assert.equal(radii.screen, 34);
  assert.ok(radii.lg > radii.md && radii.md > radii.sm);
});

test('typography: variants have valid fontSize, lineHeight and fontWeights', () => {
  const variants = Object.keys(typography) as (keyof typeof typography)[];
  assert.ok(variants.length >= 10);
  for (const v of variants) {
    const style = typography[v];
    assert.ok(typeof style.fontSize === 'number' && style.fontSize > 0);
    assert.ok(typeof style.lineHeight === 'number' && style.lineHeight >= style.fontSize);
  }
  assert.equal(typography.mono.fontFamily, 'monospace');
  assert.equal(typography.eyebrow.textTransform, 'uppercase');
});

test('shadow and layout constants', () => {
  assert.equal(TAB_BAR_HEIGHT, 64);
  assert.ok(shadow.soft.elevation && shadow.soft.elevation > 0);
  assert.ok(shadow.lift.elevation && shadow.lift.elevation > shadow.soft.elevation);
});
