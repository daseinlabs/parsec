import type { Metadata } from "next";

export const metadata: Metadata = {
  title: "Privacy",
  description:
    "Daseinlabs privacy policy: how we collect, store, protect, and share " +
    "personal information, and the rights you have over your data.",
  alternates: { canonical: "/privacy/" },
};

const CONTACT_EMAIL = "privacy@daseinlabs.ai";

// Decorative prompt caret — phosphor, hidden from assistive tech.
function Caret() {
  return (
    <span aria-hidden="true" className="mr-2 text-phosphor">
      ❯
    </span>
  );
}

function H2({ id, children }: { id: string; children: React.ReactNode }) {
  return (
    <h2
      id={`${id}-h`}
      className="mt-12 text-lg font-bold tracking-display text-ink"
    >
      <Caret />
      {children}
    </h2>
  );
}

function H3({ children }: { children: React.ReactNode }) {
  return (
    <h3 className="mt-8 text-base font-bold tracking-display text-ink">
      {children}
    </h3>
  );
}

function ContactEmail() {
  return (
    <a
      href={`mailto:${CONTACT_EMAIL}`}
      className="text-info underline underline-offset-2 hover:text-phosphor"
    >
      {CONTACT_EMAIL}
    </a>
  );
}

export default function PrivacyPage() {
  return (
    <main className="mx-auto w-full max-w-3xl px-6 py-16">
      <h1 className="text-2xl font-bold tracking-display text-ink">
        <Caret />
        Privacy Policy
      </h1>
      <p className="mt-2 text-xs text-faint">Effective date: 2026-07-28</p>

      <section id="introduction" aria-labelledby="introduction-h">
        <H2 id="introduction">Introduction and organizational info</H2>
        <p className="mt-4 text-base text-muted">
          We, at Daseinlabs, are dedicated to serving our customers and
          contacts to the best of our abilities. Part of our commitment
          involves the responsible management of personal information collected
          through our website daseinlabs.ai, and any related interactions. Our
          primary goals in processing this information include:
        </p>
        <ul className="mt-4 list-disc space-y-2 pl-5 text-base text-muted marker:text-faint">
          <li>
            Enhancing the user experience on our platform by understanding
            customer needs and preferences.
          </li>
          <li>
            Providing timely support and responding to inquiries or service
            requests.
          </li>
          <li>
            Improving our products and services to meet the evolving demands
            of our users.
          </li>
          <li>
            Conducting necessary business operations, such as billing and
            account management.
          </li>
        </ul>
        <p className="mt-4 text-base text-muted">
          It is our policy to process personal information with the utmost
          respect for privacy and security. We adhere to all relevant
          regulations and guidelines to ensure that the data we handle is
          protected against unauthorized access, disclosure, alteration, and
          destruction. Our practices are designed to safeguard the
          confidentiality and integrity of your personal information, while
          enabling us to deliver the services you trust us with.
        </p>
        <p className="mt-3 text-base text-muted">
          We do not have a designated Data Protection Officer (DPO) but remain
          fully committed to addressing your privacy concerns. Should you have
          any questions or require further information about how we manage
          personal information, please feel free to contact us at{" "}
          <ContactEmail />.
        </p>
        <p className="mt-3 text-base text-muted">
          Your privacy is our priority. We are committed to processing your
          personal information transparently and with your safety in mind.
          This commitment extends to our collaboration with third-party
          services that may process personal information on our behalf, such
          as in the case of sending invoices. Rest assured, all activities are
          conducted in strict compliance with applicable privacy laws.
        </p>
      </section>

      <section id="scope" aria-labelledby="scope-h">
        <H2 id="scope">Scope and application</H2>
        <p className="mt-4 text-base text-muted">
          Our privacy policy is designed to protect the personal information
          of all our stakeholders, including website visitors, registered
          users, and customers. Whether you are just browsing our website
          daseinlabs.ai, using our services as a registered user, or engaging
          with us as a valued customer, we ensure that your personal data is
          processed with the highest standards of privacy and security. This
          policy outlines our practices and your rights related to personal
          information.
        </p>
      </section>

      <section id="storage" aria-labelledby="storage-h">
        <H2 id="storage">Data storage and protection</H2>

        <H3>Data storage</H3>
        <ul className="mt-3 list-disc space-y-2 pl-5 text-base text-muted marker:text-faint">
          <li>
            Personal information is stored in secure servers. For services
            that require international data transfer, we ensure that such
            transfers comply with all applicable laws and maintain data
            protection standards equivalent to those in our primary location.
          </li>
          <li>
            <strong className="font-semibold text-ink">
              Data hosting partners:
            </strong>{" "}
            We partner with reputable data hosting providers committed to
            using state-of-the-art security measures. These partners are
            selected based on their adherence to stringent data protection
            standards.
          </li>
        </ul>

        <H3>Data protection measures</H3>
        <ul className="mt-3 list-disc space-y-2 pl-5 text-base text-muted marker:text-faint">
          <li>
            <strong className="font-semibold text-ink">Access control:</strong>{" "}
            Access to personal information is strictly limited to authorized
            personnel who have a legitimate business need to access the data.
            We enforce strict access controls and regularly review
            permissions.
          </li>
          <li>
            <strong className="font-semibold text-ink">
              Security audits and monitoring:
            </strong>{" "}
            Regular security audits are conducted to identify and remediate
            potential vulnerabilities. We also monitor our systems for unusual
            activities to prevent unauthorized access.
          </li>
          <li>
            <strong className="font-semibold text-ink">Encryption:</strong> To
            protect data during transfer and at rest, we employ robust
            encryption technologies.
          </li>
        </ul>

        <H3>Data processing agreements</H3>
        <p className="mt-3 text-base text-muted">
          When we share your data with third-party service providers, we do so
          under the protection of Data Processing Agreements (DPAs) that
          ensure your information is managed in accordance with GDPR and other
          relevant data protection laws. These agreements mandate that third
          parties implement adequate technical and organizational measures to
          ensure the security of your data.
        </p>

        <H3>Transparency and control</H3>
        <p className="mt-3 text-base text-muted">
          We believe in transparency and providing you with control over your
          personal information. You will always be informed about any
          significant changes to our sharing practices, and where applicable,
          you will have the option to consent to such changes.
        </p>
        <p className="mt-3 text-base text-muted">
          Your trust is important to us, and we strive to ensure that your
          personal information is disclosed only in accordance with this
          policy and when there is a justified reason to do so. For any
          queries or concerns about how we share and disclose personal
          information, please reach out to us at <ContactEmail />.
        </p>
      </section>

      <section id="rights" aria-labelledby="rights-h">
        <H2 id="rights">User rights and choices</H2>
        <p className="mt-4 text-base text-muted">
          At Daseinlabs, we recognize and respect your rights regarding your
          personal information, in accordance with the General Data Protection
          Regulation (GDPR) and other applicable data protection laws. We are
          committed to ensuring you can exercise your rights effectively.
          Below is an overview of your rights and how you can exercise them:
        </p>

        <H3>Your rights</H3>
        <ul className="mt-3 list-disc space-y-2 pl-5 text-base text-muted marker:text-faint">
          <li>
            <strong className="font-semibold text-ink">
              Right of access (Art. 15 GDPR):
            </strong>{" "}
            You have the right to request access to the personal information
            we hold about you and to obtain information about how we process
            it.
          </li>
          <li>
            <strong className="font-semibold text-ink">
              Right to rectification (Art. 16 GDPR):
            </strong>{" "}
            If you believe that any personal information we hold about you is
            incorrect or incomplete, you have the right to request its
            correction or completion.
          </li>
          <li>
            <strong className="font-semibold text-ink">
              Right to erasure (&lsquo;right to be forgotten&rsquo;) (Art. 17
              GDPR):
            </strong>{" "}
            You have the right to request the deletion of your personal
            information when it is no longer necessary for the purposes for
            which it was collected, among other circumstances.
          </li>
          <li>
            <strong className="font-semibold text-ink">
              Right to restriction of processing (Art. 18 GDPR):
            </strong>{" "}
            You have the right to request that we restrict the processing of
            your personal information under certain conditions.
          </li>
          <li>
            <strong className="font-semibold text-ink">
              Right to data portability (Art. 20 GDPR):
            </strong>{" "}
            You have the right to receive your personal information in a
            structured, commonly used, and machine-readable format and to
            transmit those data to another controller.
          </li>
          <li>
            <strong className="font-semibold text-ink">
              Right to object (Art. 21 GDPR):
            </strong>{" "}
            You have the right to object to the processing of your personal
            information, under certain conditions, including processing for
            direct marketing.
          </li>
          <li>
            <strong className="font-semibold text-ink">
              Right to withdraw consent (Art. 7(3) GDPR):
            </strong>{" "}
            Where the processing of your personal information is based on your
            consent, you have the right to withdraw that consent at any time
            without affecting the lawfulness of processing based on consent
            before its withdrawal.
          </li>
          <li>
            <strong className="font-semibold text-ink">
              Right to lodge a complaint (Art. 77 GDPR):
            </strong>{" "}
            You have the right to lodge a complaint with a supervisory
            authority if you believe our processing of your personal
            information violates applicable data protection laws.
          </li>
        </ul>

        <H3>Exercising your rights</H3>
        <p className="mt-3 text-base text-muted">
          To exercise any of these rights, please contact us at{" "}
          <ContactEmail />. We will respond to your request
          in accordance with applicable data protection laws and within the
          timeframes stipulated by those laws. Please note, in some cases, we
          may need to verify your identity as part of the process to ensure
          the security of your personal information.
        </p>
        <p className="mt-3 text-base text-muted">
          We are committed to facilitating the exercise of your rights and to
          ensuring you have full control over your personal information. If
          you have any questions or concerns about how your personal
          information is handled, please do not hesitate to get in touch with
          us.
        </p>
      </section>

      <section id="cookies" aria-labelledby="cookies-h">
        <H2 id="cookies">Cookies and tracking technologies</H2>
        <p className="mt-4 text-base text-muted">
          At Daseinlabs, we value your privacy and are committed to being
          transparent about our use of cookies and other tracking technologies
          on our website daseinlabs.ai. These technologies play a crucial role
          in ensuring the smooth operation of our digital platforms, enhancing
          your user experience, and providing insights that help us improve.
        </p>

        <H3>Understanding cookies and tracking technologies</H3>
        <p className="mt-3 text-base text-muted">
          Cookies are small data files placed on your device that enable us to
          remember your preferences and collect information about your website
          usage. Tracking technologies, such as web beacons and pixel tags,
          help us understand how you interact with our site and which pages
          you visit.
        </p>

        <H3>How we use these technologies</H3>
        <ul className="mt-3 list-disc space-y-2 pl-5 text-base text-muted marker:text-faint">
          <li>
            <strong className="font-semibold text-ink">
              Essential cookies:
            </strong>{" "}
            Necessary for the website&rsquo;s functionality, such as
            authentication and security. They do not require consent.
          </li>
          <li>
            <strong className="font-semibold text-ink">
              Performance and analytics cookies:
            </strong>{" "}
            These collect information about how visitors use our website,
            which pages are visited most frequently, and if error messages are
            received from web pages. These cookies help us improve our
            website.
          </li>
          <li>
            <strong className="font-semibold text-ink">
              Functional cookies:
            </strong>{" "}
            Enable the website to provide enhanced functionality and
            personalization, like remembering your preferences.
          </li>
          <li>
            <strong className="font-semibold text-ink">
              Advertising and targeting cookies:
            </strong>{" "}
            Used to deliver advertisements more relevant to you and your
            interests. They are also used to limit the number of times you see
            an advertisement and help measure the effectiveness of the
            advertising campaign.
          </li>
        </ul>

        <H3>Your choices and consent</H3>
        <p className="mt-3 text-base text-muted">
          Upon your first visit, our website will present you with a cookie
          consent banner, where you can:
        </p>
        <ul className="mt-3 list-disc space-y-2 pl-5 text-base text-muted marker:text-faint">
          <li>
            <strong className="font-semibold text-ink">
              Accept all cookies:
            </strong>{" "}
            Consent to the use of all cookies and tracking technologies.
          </li>
          <li>
            <strong className="font-semibold text-ink">
              Reject non-essential cookies:
            </strong>{" "}
            Only essential cookies will be used to provide you with necessary
            website functions.
          </li>
          <li>
            <strong className="font-semibold text-ink">
              Customize your preferences:
            </strong>{" "}
            Choose which categories of cookies you wish to allow.
          </li>
        </ul>
      </section>

      <section id="us-privacy" aria-labelledby="us-privacy-h">
        <H2 id="us-privacy">Compliance with United States privacy laws</H2>
        <p className="mt-4 text-base text-muted">
          To appeal a decision we may make regarding your request, please
          contact us within 60 days of receiving our response by submitting
          your request through the link on our website or by using one of the
          following methods:
        </p>
        <ul className="mt-3 list-disc space-y-2 pl-5 text-base text-muted marker:text-faint">
          <li>
            Email: <ContactEmail />
          </li>
        </ul>
        <p className="mt-3 text-base text-muted">
          In your appeal request, please include your original request, the
          date of our response, and a brief explanation of why you believe our
          decision was incorrect.
        </p>
        <p className="mt-3 text-base text-muted">
          For residents of the United States of America the following
          provisions apply:
        </p>

        <H3>A. Individual rights</H3>
        <p className="mt-3 text-base text-muted">
          The California Consumer Privacy Act provides residents of California
          specific rights regarding their personal information, additional to
          what has been described before.
        </p>

        <H3>B. Right to Know</H3>
        <p className="mt-3 text-base text-muted">
          You may request that we disclose to you what personal information we
          have collected, used, shared, or sold about you, and why we
          collected, used, shared, or sold that information. Specifically, you
          may request the disclosure of:
        </p>
        <ul className="mt-3 list-disc space-y-2 pl-5 text-base text-muted marker:text-faint">
          <li>The categories of personal information collected</li>
          <li>Specific pieces of personal information collected</li>
          <li>
            The categories of sources from which we collected personal
            information
          </li>
          <li>The purposes for which personal information is used</li>
          <li>
            The categories of third parties with whom personal information is
            shared
          </li>
          <li>
            The categories of information that are sold or disclosed to third
            parties
          </li>
        </ul>

        <H3>C. Right to Delete</H3>
        <p className="mt-3 text-base text-muted">
          You may request that we delete personal information we have
          collected about you.
        </p>

        <H3>D. Right to Correct</H3>
        <p className="mt-3 text-base text-muted">
          You may ask us to correct inaccurate information that we have about
          you.
        </p>

        <H3>E. Right to Limit</H3>
        <p className="mt-3 text-base text-muted">
          You can request us to only use your sensitive personal information
          (for example, your social security number, your genetic data, etc.)
          for limited purposes, such as providing you with the services you
          requested.
        </p>

        <H3>F. Right to Opt-Out</H3>
        <p className="mt-3 text-base text-muted">
          Daseinlabs does not sell or share personal information. In case your
          data is sold or shared you can make use of your right to opt-out of
          the sale or sharing of personal information by submitting your
          request through the link on our website.
        </p>

        <H3>G. Right to Non-Discrimination</H3>
        <p className="mt-3 text-base text-muted">
          You have the right to be protected from discrimination for
          exercising your rights.
        </p>

        <H3>H. Submitting requests</H3>
        <p className="mt-3 text-base text-muted">
          You may submit your request by sending an email to <ContactEmail />.
          We will compare the information you
          submit to us with the information we have in our records to verify
          your request. We will then respond to your request in accordance
          with the requirements.
        </p>

        <H3>J. Sensitive data and/or biometric data</H3>
        <p className="mt-3 text-base text-muted">
          We only process sensitive personal data with your prior consent and
          only for specific purposes that are clearly disclosed at the time of
          collection. You may withdraw your consent at any time by submitting
          your request through the link on our website or by email to{" "}
          <ContactEmail />.
        </p>
      </section>

      <section id="updates" aria-labelledby="updates-h">
        <H2 id="updates">Policy updates and changes</H2>
        <p className="mt-4 text-base text-muted">
          At Daseinlabs, we are committed to keeping you informed about how we
          handle your personal information and any changes to our privacy
          practices. We may update this privacy policy from time to time to
          reflect changes in legal requirements, industry standards, or our
          business operations. We want to assure you that any updates will be
          communicated transparently and in accordance with applicable data
          protection laws.
        </p>

        <H3>Notification of changes</H3>
        <ul className="mt-3 list-disc space-y-2 pl-5 text-base text-muted marker:text-faint">
          <li>
            <strong className="font-semibold text-ink">
              Notification process:
            </strong>{" "}
            In the event of significant changes to our privacy policy that may
            affect your rights or the way we handle your personal information,
            we will provide notice through prominent means, such as email,
            website notifications, or other appropriate channels. We will also
            indicate the effective date of the updated policy at the top of
            the document.
          </li>
          <li>
            <strong className="font-semibold text-ink">
              Reviewing changes:
            </strong>{" "}
            We encourage you to review our privacy policy periodically to stay
            informed about how we collect, use, and protect your personal
            information. Your continued use of our services after any changes
            to the policy signifies your acceptance of the updated terms.
          </li>
        </ul>

        <H3>Opt-in consent for material changes</H3>
        <p className="mt-3 text-base text-muted">
          For material changes to our privacy policy that require your consent
          under applicable data protection laws, such as the General Data
          Protection Regulation (GDPR), we will seek your explicit opt-in
          consent before implementing the changes. You will have the
          opportunity to review the updated policy and provide your consent
          before any changes take effect.
        </p>
      </section>

      <section id="contact" aria-labelledby="contact-h">
        <H2 id="contact">Contact us</H2>
        <p className="mt-4 text-base text-muted">
          If you have any questions or concerns about our privacy policy or
          any updates to it, please don&rsquo;t hesitate to contact us at{" "}
          <ContactEmail />. We are here to address any
          inquiries you may have and to ensure that you have the information
          you need to feel confident about how your personal information is
          handled.
        </p>
      </section>
    </main>
  );
}
