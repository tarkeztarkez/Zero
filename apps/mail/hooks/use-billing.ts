
type FeatureState = {
  total: number;
  remaining: number;
  unlimited: boolean;
  enabled: boolean;
  usage: number;
  nextResetAt: number | null;
  interval: string;
  included_usage: number;
};

// Self-hosted: there is no billing, every feature is unlimited.
const UNLIMITED: FeatureState = {
  total: 0,
  remaining: 0,
  unlimited: true,
  enabled: true,
  usage: 0,
  nextResetAt: null,
  interval: '',
  included_usage: 0,
};

const noop = async (..._args: unknown[]) => undefined;

export const useBilling = () => {
  return {
    isLoading: false,
    customer: null,
    refetch: noop,
    attach: noop,
    track: noop,
    openBillingPortal: noop,
    isPro: true,
    chatMessages: UNLIMITED,
    connections: UNLIMITED,
    brainActivity: UNLIMITED,
  };
};
