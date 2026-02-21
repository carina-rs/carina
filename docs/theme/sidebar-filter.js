(function () {
    "use strict";

    // Use setTimeout to ensure this runs after toc.js has fully processed the sidebar
    document.addEventListener("DOMContentLoaded", function () {
        setTimeout(init, 0);
    });

    function init() {
        var scrollbox = document.querySelector("mdbook-sidebar-scrollbox");
        if (!scrollbox) return;

        var providerInfo = detectProvider();

        if (providerInfo) {
            setupProviderSidebar(scrollbox, providerInfo);
        } else {
            setupTopLevelSidebar(scrollbox);
        }

        // Scroll active item into view before revealing (using scrollTop to avoid shifting parent)
        var activeLink = scrollbox.querySelector("a.active");
        if (activeLink) {
            var boxRect = scrollbox.getBoundingClientRect();
            var linkRect = activeLink.getBoundingClientRect();
            var relativeTop = linkRect.top - boxRect.top + scrollbox.scrollTop;
            scrollbox.scrollTop = relativeTop - scrollbox.clientHeight / 2;
        }

        // Reveal sidebar after processing (prevents flicker)
        scrollbox.classList.add("sidebar-ready");

        setupSidebarResize();
    }

    // Detect which provider page we're on, if any
    // Returns { name, pathPrefix, displayName } or null
    function detectProvider() {
        var path = window.location.pathname;
        var providers = [
            { name: "awscc", pathPrefix: "/providers/awscc/", displayName: "AWSCC" },
            { name: "aws", pathPrefix: "/providers/aws/", displayName: "AWS" }
        ];
        for (var i = 0; i < providers.length; i++) {
            if (path.indexOf(providers[i].pathPrefix) !== -1) {
                return providers[i];
            }
        }
        return null;
    }

    function setupTopLevelSidebar(scrollbox) {
        // On non-provider pages: hide nested resource items, show only top-level
        var sections = scrollbox.querySelectorAll("ol.section");
        sections.forEach(function (ol) {
            ol.style.display = "none";
        });
        // Hide section numbers (e.g., "1." before "AWSCC Provider")
        var sectionNumbers = scrollbox.querySelectorAll("strong[aria-hidden='true']");
        sectionNumbers.forEach(function (el) {
            el.style.display = "none";
        });
        // Also hide on-this-page headers
        var onThisPage = scrollbox.querySelector(".on-this-page");
        if (onThisPage) onThisPage.style.display = "none";
    }

    function setupProviderSidebar(scrollbox, providerInfo) {
        // On provider pages: replace sidebar with provider title + filter + resource list

        // Extract resource links from the existing sidebar before replacing
        var links = [];
        var currentCategory = null;
        var providerHeaderName = providerInfo.displayName + " Provider";

        // Get ALL chapter-items in the sidebar (flat scan, no nesting assumptions)
        var allItems = scrollbox.querySelectorAll("li.chapter-item");
        allItems.forEach(function (li) {
            var linkWrapper = li.querySelector("span.chapter-link-wrapper");
            if (!linkWrapper) return;

            var a = linkWrapper.querySelector("a");
            var span = linkWrapper.querySelector("span:not(.chapter-link-wrapper)");

            if (span && !a) {
                // Category header (EC2, S3, etc.) - has span text but no link
                var name = span.textContent.replace(/^[\d.]+\s*/, "");
                // Skip provider header (e.g., "AWSCC Provider", "AWS Provider")
                if (name === providerHeaderName) return;
                currentCategory = { name: name, resources: [] };
                links.push(currentCategory);
            } else if (a && currentCategory) {
                // Resource link under a category
                var href = a.getAttribute("href");
                if (href && href.indexOf("providers/" + providerInfo.name + "/") !== -1) {
                    currentCategory.resources.push({
                        text: a.textContent.replace(/^[\d.]+\s*/, ""),
                        href: href,
                        active: a.classList.contains("active")
                    });
                }
            }
        });

        // Remove empty categories (from other providers' sections in the TOC)
        links = links.filter(function (cat) { return cat.resources.length > 0; });

        // Build header (title + filter) outside scrollbox
        var header = document.createElement("div");
        header.className = "sidebar-header";

        var title = document.createElement("div");
        title.className = "sidebar-provider-title";
        title.textContent = providerInfo.displayName;
        header.appendChild(title);

        var input = document.createElement("input");
        input.type = "text";
        input.id = "sidebar-filter-input";
        input.placeholder = "Filter resources... (Ctrl+K)";
        input.setAttribute("aria-label", "Filter resources");
        header.appendChild(input);

        // Insert header before scrollbox in the sidebar nav
        scrollbox.parentNode.insertBefore(header, scrollbox);

        // Push scrollbox down below the header (scrollbox is position:absolute top:0)
        var headerHeight = header.offsetHeight;
        scrollbox.style.top = headerHeight + "px";

        // Build resource list inside scrollbox
        var list = document.createElement("div");
        list.className = "provider-resource-list";

        links.forEach(function (category) {
            var catDiv = document.createElement("div");
            catDiv.className = "provider-category";

            var catTitle = document.createElement("div");
            catTitle.className = "provider-category-title";
            catTitle.textContent = category.name;
            catDiv.appendChild(catTitle);

            var ul = document.createElement("ul");
            category.resources.forEach(function (res) {
                var li = document.createElement("li");
                li.className = "provider-resource-item";
                var a = document.createElement("a");
                a.href = res.href;
                a.textContent = res.text;
                if (res.active) a.classList.add("active");
                li.appendChild(a);
                ul.appendChild(li);
            });
            catDiv.appendChild(ul);
            list.appendChild(catDiv);
        });

        // Replace scrollbox content with just the resource list
        scrollbox.innerHTML = "";
        scrollbox.appendChild(list);

        // Set up filtering
        input.addEventListener("input", function () {
            var term = input.value.trim().toLowerCase();
            var categories = list.querySelectorAll(".provider-category");

            categories.forEach(function (catDiv) {
                var items = catDiv.querySelectorAll(".provider-resource-item");
                var anyVisible = false;

                items.forEach(function (li) {
                    if (!term) {
                        li.classList.remove("sidebar-filter-hidden");
                        anyVisible = true;
                    } else {
                        var text = li.textContent.toLowerCase();
                        if (text.indexOf(term) !== -1) {
                            li.classList.remove("sidebar-filter-hidden");
                            anyVisible = true;
                        } else {
                            li.classList.add("sidebar-filter-hidden");
                        }
                    }
                });

                if (!term || anyVisible) {
                    catDiv.classList.remove("sidebar-filter-hidden");
                } else {
                    catDiv.classList.add("sidebar-filter-hidden");
                }
            });
        });

        // Keyboard shortcut: Ctrl+K / Cmd+K
        document.addEventListener("keydown", function (e) {
            if ((e.ctrlKey || e.metaKey) && e.key === "k") {
                e.preventDefault();
                input.focus();
                input.select();
            }
        });
    }

    function setupSidebarResize() {
        var handle = document.getElementById("mdbook-sidebar-resize-handle");
        var sidebar = document.getElementById("mdbook-sidebar");
        if (!handle || !sidebar) return;

        var isResizing = false;

        handle.addEventListener("mousedown", function (e) {
            isResizing = true;
            document.documentElement.classList.add("sidebar-resizing");
            e.preventDefault();
        });

        window.addEventListener("mousemove", function (e) {
            if (!isResizing) return;
            var pos = e.clientX - sidebar.offsetLeft;
            if (pos < 20) {
                pos = 20;
            }
            pos = Math.min(pos, window.innerWidth - 100);
            document.documentElement.style.setProperty("--sidebar-target-width", pos + "px");
        });

        window.addEventListener("mouseup", function () {
            if (!isResizing) return;
            isResizing = false;
            document.documentElement.classList.remove("sidebar-resizing");
        });
    }
})();
