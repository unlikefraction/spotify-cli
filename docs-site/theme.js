const theme = matchMedia("(prefers-color-scheme: dark)");
const applyTheme = () => { document.documentElement.dataset.theme = theme.matches ? "dark" : "light"; };
applyTheme();
theme.addEventListener("change", applyTheme);
