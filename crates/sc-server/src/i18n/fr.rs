//! French.
//!
//! Uses `vous`, not `tu`. This is a stranger filing a request with somebody
//! else's project, which is exactly the register `vous` is for — and `tu` would
//! read as presumptuous from software the reader has never used before.
//!
//! Typographic detail worth keeping: French puts a space before `?` and `:`, and
//! it should be a **non-breaking** one (`\u{a0}`) or the punctuation wraps onto
//! its own line at a narrow width — which is a phone, which is this surface's
//! premise.

use super::Strings;

pub static STRINGS: Strings = Strings {
    // A product name, not a word. Left as it is, deliberately.
    brand: "Smart Coder",
    theme_label: "Thème",
    theme_auto: "Système",
    theme_light: "Clair",
    theme_dark: "Sombre",
    language_label: "Langue",
    language_apply: "Appliquer",
    footer_tagline: "Smart Coder — déposez une demande, lisez la spécification qui en naît.",

    signin_title: "Se connecter",
    signin_intro: "Déposer une demande nécessite une adresse e-mail — c'est ce qui vous \
                   permet de retrouver ce que vous avez déposé, et cela évite que ce \
                   formulaire ne soit ouvert à tous les vents.",
    signin_email_label: "E-mail",
    signin_email_placeholder: "vous@exemple.com",
    signin_submit: "Envoyez-moi un lien",
    signin_no_password: "Pas de mot de passe. Nous envoyons un lien à usage unique, \
                         valable quinze minutes.",

    sent_title: "Consultez votre boîte mail",
    sent_body: "Si cette adresse peut recevoir du courrier, un lien de connexion est en \
                route. Il expire dans quinze minutes.",
    sent_nothing_yet: "Rien d'autre ne s'est encore produit — c'est le lien qui vous connecte.",

    confirm_title: "Confirmer la connexion",
    confirm_intro: "Appuyez sur le bouton pour terminer la connexion sur cet appareil.",
    confirm_submit: "Connectez-moi",
    confirm_not_you: "Si vous n'êtes pas à l'origine de cette demande, fermez la page — \
                      rien ne se passe tant que vous n'appuyez pas.",

    link_failed_title: "Ce lien n'a pas fonctionné",
    link_already_used: "Ce lien a déjà été utilisé. Vous êtes probablement déjà connecté — ",
    link_already_used_link: "essayez de déposer une demande",
    link_expired: "Ce lien n'est plus valable. Ils expirent au bout de quinze minutes.",
    link_ask_again: "En demander un nouveau",

    file_title: "Déposer une demande",
    file_prompt: "Que faut-il faire\u{a0}?",
    file_placeholder: "Décrivez-le comme vous le feriez à un collègue.",
    file_submit: "Déposer",
    file_cap_before: "Jusqu'à ",
    file_cap_after: " mots. Plus c'est court, mieux c'est — une spécification est rédigée \
                     à partir de ce que vous écrivez, elle n'en est pas la copie.",
    file_spec_note: " Vous pourrez lire la spécification qui en résultera.",
    file_kind_label: "Type",
    file_mine_heading: "Ce que vous avez déposé",
    file_nothing_yet: "Vous n'avez encore rien déposé.",
    file_signout: "Se déconnecter",

    filed_title: "Déposée",
    filed_body: "Déposée. Revenez sur cette page pour suivre ce qu'elle devient.",
    filed_feedback_body: "Merci — c'est enregistré. Les retours sont conservés pour que le \
                          développeur les lise\u{a0}; ils ne deviennent pas des spécifications.",
    back: "Retour",

    detail_asked_heading: "Ce que vous avez demandé",
    detail_spec_heading: "La spécification obtenue",
    detail_spec_withheld: "Une spécification a été rédigée et se trouve chez un relecteur.",
    detail_filed_prefix: "déposée ",

    state_received: "reçue",
    state_writing: "en cours de rédaction",
    state_reviewing: "chez un relecteur",
    state_accepted: "acceptée",
    state_closed: "close",

    // "il y a 5 min" — the marker goes in front in French, which is why these
    // are a prefix/suffix pair rather than a suffix.
    ago_just_now: "à l'instant",
    ago_prefix: "il y a ",
    ago_minutes: " min",
    ago_hours: " h",
    ago_days: " jours",

    error_empty: "Une demande a besoin d'un texte.",
    error_too_long: "C'est plus long que ce que ce formulaire accepte.",
    not_found_title: "Introuvable",
    not_found_body: "Il n'y a rien ici.",
};
