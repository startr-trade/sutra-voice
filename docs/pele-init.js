// Draw the book's diagrams with Pele.
//
// mdBook renders a ```mermaid block as `pre > code.language-mermaid`. Each becomes a `div.diagram`
// holding Pele's SVG. Pele colours its SVG with CSS variables, which `pele-theme.css` maps onto the
// Sutra palette per book theme, so switching theme needs no re-render. A block Pele cannot draw
// keeps its source and says why above it, never a blank where a diagram should be.
(() => {
    const draw = () => {
        for (const code of document.querySelectorAll('pre > code.language-mermaid')) {
            const pre = code.parentElement;
            const source = code.textContent;
            try {
                pele.render(source); // parse first: `mount` would replace the block with the bare message
            } catch (e) {
                const note = document.createElement('p');
                note.className = 'diagram-error';
                note.textContent = `This diagram could not be drawn: ${e && e.message ? e.message : e}`;
                pre.before(note);
                continue;
            }
            const box = document.createElement('div');
            box.className = 'diagram';
            pre.replaceWith(box);
            pele.mount(box, source);
        }
    };
    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', draw);
    } else {
        draw();
    }
})();
