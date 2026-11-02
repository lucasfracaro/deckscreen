// Minimal typings for what Decky Loader exposes at runtime.
// DeckScreen is built without npm packages: React, @decky/ui (window.DFL) and the
// plugin API are taken straight from the globals the loader sets up.

declare namespace JSX {
  interface IntrinsicElements {
    [elem: string]: any;
  }
  type Element = any;
}

interface Window {
  SP_REACT: any;
  DFL: any;
  appStore?: any;
  __DECKY_SECRET_INTERNALS_DO_NOT_USE_OR_YOU_WILL_BE_FIRED_deckyLoaderAPIInit?: {
    connect: (version: number, pluginName: string) => any;
  };
}

declare const SteamClient: any;
