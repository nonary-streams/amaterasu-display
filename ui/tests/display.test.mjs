import assert from 'node:assert/strict';
import test from 'node:test';
import vm from 'node:vm';
import { readFileSync } from 'node:fs';
const source = readFileSync(new URL('../display.js', import.meta.url), 'utf8');
const settle = async () => { for (let i = 0; i < 50; i++)
    await Promise.resolve(); };
async function fixture({ application = false, managed = false, canWrite = true } = {}) {
    let registration, revision = 7, dirty = false, busy = false, timer = 0, notifications = 0;
    let host = { configured_output_name: 'screen-a', configured_display_policy: { dd_configuration_option: 'verify_only', adapter_pnp_id: 'keep-adapter' } };
    let entry = { name: 'Game', launch: { kind: managed ? 'playnite' : 'executable', program: '/game' }, preparation: [{ command: 'keep' }], artwork: { provider: 'external', reference: 'keep' }, config_overrides: { fec_percentage: 20, display_policy: { dd_configuration_option: 'verify_only', adapter_pnp_id: 'keep-adapter' } } };
    const timers = new Map(), controls = [], writes = [], controller = new AbortController();
    const element = () => ({ children: [], append(...items) { this.children.push(...items); }, replaceChildren(...items) { this.children = items; }, setAttribute() { } });
    const context = vm.createContext({ document: { createElement: element }, window: { AmaterasuExtensions: { v1: { register(id, page) { assert.equal(id, 'display'); registration = page; } } } }, setTimeout(callback) { timers.set(++timer, callback); return timer; }, clearTimeout(id) { timers.delete(id); } });
    vm.runInContext(source, context);
    const make = (kind) => (options) => { if (kind === 'select') {
        assert.ok(options.choices.every(choice => typeof choice.value === 'string' && choice.value));
        assert.ok(options.choices.some(choice => choice.value === options.value));
    } let value = options.value; const control = { kind, options, element: element(), disposed: false, value: () => value, setValue(next) { value = next; }, dispose() { this.disposed = true; }, edit(next) { value = next; options.onChange(next); } }; controls.push(control); return control; };
    const sdk = { signal: controller.signal, ...(application ? { application: { id: 'app-1', name: 'Game', notifyChanged() { notifications++; } } } : {}), canCall: alias => !alias.includes('update') || canWrite, setDirty(value) { dirty = value; }, setBusy(value) { busy = value; },
        ui: Object.fromEntries(['toggle', 'select', 'slider', 'numberField', 'textField', 'button'].map(kind => [kind, make(kind)])),
        async call(alias, payload) {
            if (alias === 'host_read')
                return { ok: true, revision, ...structuredClone(host) };
            if (alias === 'app_read')
                return { ok: true, revision, id: 'app-1', entry: structuredClone(entry) };
            assert.equal(payload.expected_revision, revision);
            writes.push({ alias, payload: structuredClone(payload) });
            if (alias === 'host_update') {
                host.configured_output_name = payload.values.output_name ?? '';
                for (const [key, value] of Object.entries(payload.values.display_policy)) {
                    if (value == null)
                        delete host.configured_display_policy[key];
                    else
                        host.configured_display_policy[key] = value;
                }
            }
            else if (alias === 'app_update')
                entry = structuredClone(payload.entry);
            else if (alias === 'app_update_provider')
                entry.config_overrides = structuredClone(payload.config_overrides);
            else
                throw Error(alias);
            return { ok: true, revision: ++revision };
        } };
    registration[application ? 'mountApplication' : 'mountSettings'](element(), sdk);
    await settle();
    return { controls, writes, controller, dirty: () => dirty, busy: () => busy, entry: () => entry, host: () => host, notifications: () => notifications, async flush() { const callbacks = [...timers.values()]; timers.clear(); callbacks.forEach(callback => callback()); await settle(); } };
}
test('host display writes only display fields and preserves unedited policy keys', async () => {
    const env = await fixture();
    env.controls.find(control => control.options.label === 'Display preparation').edit('ensure_primary');
    await env.flush();
    assert.equal(env.writes[0].alias, 'host_update');
    assert.deepEqual(Object.keys(env.writes[0].payload.values).sort(), ['display_policy', 'output_name']);
    assert.equal(env.host().configured_display_policy.adapter_pnp_id, 'keep-adapter');
    assert.equal(env.dirty(), false);
});
test('application save preserves launch, artwork, preparation and unrelated overrides', async () => {
    const env = await fixture({ application: true });
    const before = structuredClone(env.entry());
    env.controls.find(control => control.options.label === 'Display preparation').edit('ensure_primary');
    await env.flush();
    assert.equal(env.writes[0].alias, 'app_update');
    assert.deepEqual(env.entry().launch, before.launch);
    assert.deepEqual(env.entry().preparation, before.preparation);
    assert.deepEqual(env.entry().artwork, before.artwork);
    assert.equal(env.entry().config_overrides.fec_percentage, 20);
    assert.equal(env.entry().config_overrides.display_policy.adapter_pnp_id, 'keep-adapter');
    assert.equal(env.notifications(), 1);
    assert.equal(env.dirty(), false);
});
test('provider entries update only overrides and invalid JSON never saves stale values', async () => {
    const env = await fixture({ application: true, managed: true });
    env.controls.find(control => control.options.label === 'Display preparation').edit('ensure_primary');
    await env.flush();
    assert.equal(env.writes[0].alias, 'app_update_provider');
    const invalid = await fixture();
    invalid.controls.find(control => control.options.label === 'Display preparation').edit('ensure_active');
    invalid.controls.find(control => control.options.label === 'Advanced display policy (JSON)').edit('{bad');
    await invalid.flush();
    await invalid.controls.find(control => control.kind === 'button').options.onClick();
    assert.equal(invalid.writes.length, 0);
    assert.equal(invalid.dirty(), true);
    invalid.controller.abort();
    assert.ok(invalid.controls.every(control => control.disposed));
});
test('read-only controls stay disabled and cannot schedule writes', async () => {
    const env = await fixture({ canWrite: false });
    assert.ok(env.controls.every(control => control.options.disabled));
    env.controls.find(control => control.options.label === 'Display preparation').edit('ensure_active');
    await env.flush();
    assert.equal(env.writes.length, 0);
});
test('application display UI preserves explicit ten-bit SDR preferences and supports inheritance', async () => {
    const env = await fixture({ application: true });
    const preference = env.controls.find(control => control.options.label === '10-bit SDR preference');
    preference.edit('true');
    await env.flush();
    assert.equal(env.entry().config_overrides.prefer_10bit_sdr, true);
    preference.edit('inherit');
    await env.flush();
    assert.equal(Object.hasOwn(env.entry().config_overrides, 'prefer_10bit_sdr'), false);
});
