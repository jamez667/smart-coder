document.addEventListener('DOMContentLoaded', function () {
  // Scroll to menu on CTA click
  const ctaButton = document.getElementById('cta');
  if (ctaButton) {
    ctaButton.addEventListener('click', function () {
      const menuSection = document.getElementById('menu');
      if (menuSection) {
        menuSection.scrollIntoView({ behavior: 'smooth' });
      }
    });
  }

  // Contact form submission handling
  const contactForm = document.getElementById('contact-form');
  if (contactForm) {
    contactForm.addEventListener('submit', function (event) {
      event.preventDefault();
      const nameInput = contactForm.querySelector('#name');
      const emailInput = contactForm.querySelector('#email');
      const name = nameInput ? nameInput.value.trim() : '';
      const email = emailInput ? emailInput.value.trim() : '';

      if (name && email) {
        alert(`Thanks, ${name}! We'll be in touch.`);
        contactForm.reset();
      } else {
        alert('Please fill in your name and email.');
      }
    });
  }
});
