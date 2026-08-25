export {};

declare global {
  namespace NodeJS {
    interface ProcessEnv {
      readonly RECEIPT?: string;
      readonly PROD_DEPLOY_RECEIPT?: string;
    }
  }
}
