import type { ModelOption } from './types';

export interface ModelProvider { id: string; name: string; models: ModelOption[]; }

function providerLabel(id: string): string {
  if (id === '__default__') return '默认提供商';
  return id.replace(/[_-]+/g, ' ').replace(/\b\w/g, letter => letter.toUpperCase());
}

export function modelProviders(models: ModelOption[]): ModelProvider[] {
  const groups = new Map<string, ModelProvider>();
  for (const model of models) {
    const id = model.provider_id || '__default__';
    const group = groups.get(id) || { id, name: model.provider_name || providerLabel(id), models: [] };
    if (model.provider_name) group.name = model.provider_name;
    group.models.push(model);
    groups.set(id, group);
  }
  return [...groups.values()];
}

export function selectedProvider(models: ModelOption[], modelId: string | null | undefined): string {
  const selected = models.find(item => item.id === modelId);
  return selected?.provider_id || modelProviders(models)[0]?.id || '__default__';
}

export function providerOptionsHtml(models: ModelOption[], selected: string, escape: (value: string) => string): string {
  return modelProviders(models).map(provider => `<option value="${escape(provider.id)}" ${provider.id === selected ? 'selected' : ''}>${escape(provider.name)}</option>`).join('');
}

export function modelOptionsHtml(models: ModelOption[], provider: string, selected: string | null | undefined, escape: (value: string) => string, emptyLabel = '默认'): string {
  const providers = modelProviders(models);
  const choices = providers.length > 1 ? (providers.find(item => item.id === provider)?.models || []) : models;
  return `<option value="">${escape(emptyLabel)}</option>${choices.map(item => `<option value="${escape(item.id)}" ${item.id === selected ? 'selected' : ''}>${escape(item.name)}</option>`).join('')}`;
}
