// Display owns these controls. Storage and native effects stay behind approved APIs.
(() => {
    const fields = [
        ['dd_configuration_option', 'Display preparation', ['disabled', 'verify_only', 'ensure_active', 'ensure_primary', 'ensure_only_display']],
        ['dd_resolution_option', 'Resolution', ['disabled', 'auto', 'manual']],
        ['dd_manual_resolution', 'Manual resolution', null],
        ['dd_refresh_rate_option', 'Refresh rate', ['disabled', 'auto', 'manual', 'prefer_highest']],
        ['dd_manual_refresh_rate', 'Manual refresh rate', null],
        ['dd_hdr_option', 'Display HDR', ['disabled', 'auto']],
        ['dd_hdr_request_override', 'HDR request', ['follow_client', 'force_on', 'force_off']],
        ['adapter_name', 'Graphics adapter', null],
        ['virtual_display_mode', 'Virtual display mode', ['disabled', 'per_client', 'shared']],
        ['virtual_display_layout', 'Virtual display layout', ['exclusive', 'extended', 'extended_primary', 'extended_isolated', 'extended_primary_isolated']],
        ['dd_virtual_display_scale', 'Virtual display scale (%)', 'number'],
    ];
    const label = value => ({ auto: 'Automatic', verify_only: 'Verify only', ensure_active: 'Activate display', ensure_primary: 'Make primary', ensure_only_display: 'Use only this display', per_client: 'Per client', prefer_highest: 'Highest available', follow_client: 'Follow client', force_on: 'Force HDR', force_off: 'Force SDR' })[value] ?? value.replaceAll('_', ' ');
    const clone = value => JSON.parse(JSON.stringify(value));
    const canonical = value => JSON.stringify(value && typeof value === 'object' && !Array.isArray(value)
        ? Object.fromEntries(Object.keys(value).sort().map(key => [key, JSON.parse(canonical(value[key]))])) : value);
    const same = (a, b) => canonical(a) === canonical(b);
    const sparse = value => Object.fromEntries(Object.entries(value ?? {}).filter(([, value]) => value != null));
    const checked = value => {
        if (!value || typeof value !== 'object' || value.ok === false || value.error)
            throw new Error(value?.error?.message ?? value?.message ?? value?.code ?? 'Display request failed.');
        if (!Number.isSafeInteger(value.revision) || value.revision < 0)
            throw new Error('Invalid display settings revision.');
        return value;
    };
    const appValues = entry => {
        const policy = sparse(entry.config_overrides?.display_policy);
        for (const id of ['dd_refresh_rate_option', 'dd_manual_refresh_rate'])
            if (entry.config_overrides?.[id] != null)
                policy[id] = entry.config_overrides[id];
        return { output: entry['display-output'] ?? '', virtual: entry['virtual-screen'] ?? null,
            mode: entry['virtual-display-mode'] ?? '', layout: entry['virtual-display-layout'] ?? '',
            prefer10Bit: entry.config_overrides?.prefer_10bit_sdr ?? null, policy };
    };
    const hostValues = reply => ({ output: reply.configured_output_name ?? '', policy: sparse(reply.configured_display_policy) });
    const mount = (container, sdk) => {
        const application = sdk.application;
        const canRead = sdk.canCall(application ? 'app_read' : 'host_read');
        const canWrite = sdk.canCall(application ? 'app_update' : 'host_update');
        const status = document.createElement('p');
        status.setAttribute('role', 'status');
        const title = document.createElement('h2');
        title.textContent = 'Display';
        const help = document.createElement('p');
        help.textContent = 'Choose display preparation for the next stream. Saving settings does not change an active display lease or restart a stream.';
        const rows = document.createElement('div');
        container.replaceChildren(title, help, rows, status);
        const controls = [], reflectors = [];
        let baseline, draft, saving = false, generation = 0, timer, disposed = false, policyText, invalidPolicy = false;
        const dirty = () => draft && !same(draft, baseline);
        const updateDirty = () => sdk.setDirty(!!dirty() || invalidPolicy);
        const cancelTimer = () => { clearTimeout(timer); timer = undefined; };
        const schedule = () => { cancelTimer(); if (!disposed && dirty() && !invalidPolicy)
            timer = setTimeout(() => { void save(); }, 300); };
        const changed = callback => { if (disposed || !canWrite)
            return; callback(); generation++; updateDirty(); status.textContent = 'Display changes waiting to save…'; schedule(); };
        const read = async () => {
            const value = checked(await sdk.call(application ? 'app_read' : 'host_read', application ? { id: application.id } : {}));
            if (application && (value.id !== application.id || !value.entry || typeof value.entry !== 'object'))
                throw new Error('Application display settings could not be correlated.');
            return value;
        };
        const values = reply => application ? appValues(reply.entry) : hostValues(reply);
        function refreshControls() { for (const reflect of reflectors)
            reflect(); if (policyText)
            policyText.setValue(JSON.stringify(draft.policy, null, 2)); }
        async function save() {
            cancelTimer();
            if (saving || disposed || !dirty() || !canWrite || invalidPolicy)
                return;
            saving = true;
            sdk.setBusy(true);
            const submitted = clone(draft), submittedGeneration = generation;
            let confirmed = false;
            status.textContent = 'Saving display settings…';
            try {
                const current = await read();
                if (disposed)
                    return;
                if (!same(values(current), baseline))
                    throw new Error('Display settings changed elsewhere. Reload before saving.');
                let result;
                if (application) {
                    const entry = clone(current.entry), overrides = { ...entry.config_overrides };
                    overrides.display_policy = submitted.policy;
                    if (submitted.prefer10Bit == null)
                        delete overrides.prefer_10bit_sdr;
                    else
                        overrides.prefer_10bit_sdr = submitted.prefer10Bit;
                    delete overrides.dd_refresh_rate_option;
                    delete overrides.dd_manual_refresh_rate;
                    if (entry.launch?.kind === 'playnite') {
                        if (!sdk.canCall('app_update_provider'))
                            throw new Error('Provider display settings are read-only.');
                        result = await sdk.call('app_update_provider', { id: application.id, expected_revision: current.revision, config_overrides: overrides });
                    }
                    else {
                        entry.config_overrides = overrides;
                        for (const [key, value] of [['display-output', submitted.output], ['virtual-screen', submitted.virtual], ['virtual-display-mode', submitted.mode], ['virtual-display-layout', submitted.layout]]) {
                            if (value === '' || value == null)
                                delete entry[key];
                            else
                                entry[key] = value;
                        }
                        result = await sdk.call('app_update', { id: application.id, expected_revision: current.revision, entry });
                    }
                }
                else {
                    const policyPatch = Object.fromEntries([...new Set([...Object.keys(baseline.policy), ...Object.keys(submitted.policy)])].map(key => [key, submitted.policy[key] ?? null]));
                    result = await sdk.call('host_update', { expected_revision: current.revision, values: { output_name: submitted.output || null, display_policy: policyPatch } });
                }
                if (disposed)
                    return;
                checked(result);
                if (result.revision <= current.revision)
                    throw new Error('Save revision was not confirmed.');
                const refreshed = await read();
                if (disposed)
                    return;
                if (refreshed.revision !== result.revision || !same(values(refreshed), submitted))
                    throw new Error('Saved display values could not be confirmed. Reload before retrying.');
                baseline = submitted;
                confirmed = true;
                application?.notifyChanged?.();
                if (generation === submittedGeneration)
                    refreshControls();
                updateDirty();
                status.textContent = 'Display settings saved for the next stream.';
            }
            catch (error) {
                if (!disposed) {
                    status.textContent = `Unable to confirm display settings: ${error.message}`;
                    sdk.setDirty(true);
                }
            }
            finally {
                saving = false;
                if (!disposed) {
                    sdk.setBusy(false);
                    if (confirmed && generation !== submittedGeneration)
                        schedule();
                }
            }
        }
        const add = control => { controls.push(control); rows.append(control.element); return control; };
        const select = (name, choices, get, set) => {
            const control = add(sdk.ui.select({ label: name, choices: [{ value: 'inherit', label: application ? 'Inherit' : 'Default' }, ...choices.map(value => ({ value, label: label(value) }))], value: get() || 'inherit', disabled: !canWrite,
                onChange: value => changed(() => { set(value === 'inherit' ? '' : value); if (policyText)
                    policyText.setValue(JSON.stringify(draft.policy, null, 2)); }) }));
            reflectors.push(() => control.setValue(get() || 'inherit'));
            return control;
        };
        const text = (name, get, set) => {
            const control = add(sdk.ui.textField({ label: name, value: get() ?? '', maxLength: 1024, disabled: !canWrite, onChange: value => changed(() => { set(value); if (policyText)
                    policyText.setValue(JSON.stringify(draft.policy, null, 2)); }) }));
            reflectors.push(() => control.setValue(get() ?? ''));
            return control;
        };
        const saveButton = sdk.ui.button({ label: 'Save display settings', disabled: !canWrite, onClick: save });
        controls.push(saveButton);
        container.append(saveButton.element);
        if (!canRead) {
            status.textContent = 'Display settings are unavailable for this account.';
            return () => saveButton.dispose();
        }
        status.textContent = 'Loading display settings…';
        void read().then(reply => {
            if (disposed)
                return;
            draft = values(reply);
            baseline = clone(draft);
            const managed = application && reply.entry.launch?.kind === 'playnite';
            if (!managed) {
                text('Display output', () => draft.output, value => { draft.output = value; });
                if (application) {
                    select('10-bit SDR preference', ['true', 'false'], () => draft.prefer10Bit == null ? '' : String(draft.prefer10Bit), value => { draft.prefer10Bit = value === '' ? null : value === 'true'; });
                    select('Use a virtual screen', ['true', 'false'], () => draft.virtual == null ? '' : String(draft.virtual), value => { draft.virtual = value === '' ? null : value === 'true'; });
                    select('Virtual display mode', ['disabled', 'per_client', 'shared'], () => draft.mode, value => { draft.mode = value; });
                    select('Virtual display layout', ['exclusive', 'extended', 'extended_primary', 'extended_isolated', 'extended_primary_isolated'], () => draft.layout, value => { draft.layout = value; });
                }
            }
            for (const [id, name, choices] of fields) {
                // App top-level mode/layout are distinct from provider display policy.
                if (application && !managed && ['virtual_display_mode', 'virtual_display_layout'].includes(id))
                    continue;
                const set = value => { if (value === '')
                    delete draft.policy[id];
                else
                    draft.policy[id] = value; };
                if (Array.isArray(choices))
                    select(name, choices, () => draft.policy[id], set);
                else if (choices === 'number') {
                    const control = add(sdk.ui.numberField({ label: name, value: Number(draft.policy[id] ?? 100), minimum: 0, maximum: 65535, step: 1, disabled: !canWrite, onChange: value => changed(() => { draft.policy[id] = value; policyText?.setValue(JSON.stringify(draft.policy, null, 2)); }) }));
                    reflectors.push(() => control.setValue(Number(draft.policy[id] ?? 100)));
                    add(sdk.ui.toggle({ label: `Use inherited ${name.toLowerCase()}`, value: !Object.hasOwn(draft.policy, id), disabled: !canWrite, onChange: value => changed(() => { if (value)
                            delete draft.policy[id];
                        else
                            draft.policy[id] = control.value(); policyText?.setValue(JSON.stringify(draft.policy, null, 2)); }) }));
                }
                else
                    text(name, () => draft.policy[id], set);
            }
            policyText = add(sdk.ui.textField({ label: 'Advanced display policy (JSON)', value: JSON.stringify(draft.policy, null, 2), maxLength: 32768, disabled: !canWrite, onChange: value => {
                    try {
                        const parsed = JSON.parse(value || '{}');
                        if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed))
                            throw new Error('Use a JSON object.');
                        invalidPolicy = false;
                        changed(() => { draft.policy = sparse(parsed); });
                        for (const reflect of reflectors)
                            reflect();
                    }
                    catch (error) {
                        invalidPolicy = true;
                        cancelTimer();
                        generation++;
                        sdk.setDirty(true);
                        status.textContent = `Display policy is invalid: ${error.message}`;
                    }
                } }));
            updateDirty();
            status.textContent = canWrite ? 'Display settings are up to date.' : 'Display settings are read-only.';
        }).catch(error => { if (!disposed)
            status.textContent = `Unable to load display settings: ${error.message}`; });
        const dispose = () => { if (disposed)
            return; disposed = true; cancelTimer(); for (const control of controls)
            control.dispose(); };
        sdk.signal.addEventListener('abort', dispose, { once: true });
        return dispose;
    };
    window.AmaterasuExtensions.v1.register('display', { title: 'Display', mount, mountSettings: mount, mountApplication: mount });
})();
