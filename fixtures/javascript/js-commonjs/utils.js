// CommonJS utilities module
function helper() {
    return "commonjs helper";
}

function unusedHelper() {
    return "unused commonjs helper";
}

// Export using module.exports
module.exports = {
    helper: helper,
    CONST: 42
};