// Package root - re-exports
export { add, subtract } from './math.js';
export { greet } from './strings.js';

// Local function
function localHelper() {
    return "local";
}

// Unused in this module
function unusedInIndex() {
    return "dead";
}
