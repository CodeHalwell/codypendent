/* eslint-env node */
module.exports = {
  root: true,
  parser: "@typescript-eslint/parser",
  parserOptions: {
    ecmaVersion: 2022,
    sourceType: "module",
  },
  plugins: ["@typescript-eslint"],
  extends: ["eslint:recommended", "plugin:@typescript-eslint/recommended"],
  env: {
    node: true,
    es2022: true,
  },
  ignorePatterns: ["dist/**", "node_modules/**", "*.cjs", "*.mjs"],
  rules: {
    "@typescript-eslint/no-explicit-any": "error",
    "@typescript-eslint/no-unused-vars": [
      "error",
      { argsIgnorePattern: "^_", varsIgnorePattern: "^_" },
    ],
    "@typescript-eslint/explicit-module-boundary-types": "off",
    // The DaemonClient uses the standard typed-EventEmitter idiom: an interface
    // of the same name declares strongly-typed on/once/off/emit overloads that
    // merge onto the class. This is safe and intentional here.
    "@typescript-eslint/no-unsafe-declaration-merging": "off",
    eqeqeq: ["error", "smart"],
    "no-console": "off",
  },
};
