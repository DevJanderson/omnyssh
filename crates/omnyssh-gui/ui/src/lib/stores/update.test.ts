import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { beforeEach, describe, expect, it } from 'vitest';
import { get } from 'svelte/store';
import type { UpdateInfoDto } from '$lib/bindings';
import { availableUpdate, dismissUpdate, offerUpdate } from './update';
import { applyUpdateAvailable } from '$lib/ipc/router';

const info: UpdateInfoDto = {
  version: '1.2.0',
  url: 'https://github.com/timhartmann7/omnyssh/releases/tag/v1.2.0',
  tag: 'v1.2.0',
  canSelfUpdate: true
};

describe('update banner store', () => {
  beforeEach(() => dismissUpdate());

  it('starts hidden', () => {
    expect(get(availableUpdate)).toBeNull();
  });

  it('offerUpdate shows the update; dismiss hides it', () => {
    offerUpdate(info);
    expect(get(availableUpdate)).toEqual(info);
    dismissUpdate();
    expect(get(availableUpdate)).toBeNull();
  });

  it('applyUpdateAvailable routes an update-available event into the banner', () => {
    applyUpdateAvailable({ info });
    expect(get(availableUpdate)).toEqual(info);
  });
});

// There is no component-test harness here, so the banner's action is guarded at the
// source — this catches the literal shape only, not what the button renders. Until
// `plugins.updater` has endpoints (§3.7, Stage 5) `install_update` fails on every
// platform, so wiring the banner to it puts back a button that cannot work.
describe('update banner action', () => {
  const banner = readFileSync(
    fileURLToPath(new URL('../components/UpdateBanner.svelte', import.meta.url)),
    'utf8'
  );

  it('links to the release page and does not call the unconfigured self-update', () => {
    expect(banner).toMatch(/download\(info\.url\)/);
    expect(banner).not.toMatch(/installUpdate/);
  });
});
